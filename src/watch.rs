//! `craft watch` — the numbers, checked against their own recent normal.
//!
//! The dashboard answers "how is the site doing" when somebody thinks to ask.
//! This answers the question nobody is awake to ask: did something break. It
//! compares the most recent complete day against the mean of the days before
//! it and reports what moved further than it usually does, which needs no
//! configuration to be useful — a site's own history is the threshold.
//!
//! Three things it deliberately does not do. It does not decide whether a
//! move was good, only how far it went and in which direction. It does not
//! read a webhook URL out of `config.toml`, which the README calls safe to
//! commit to a dotfile repo — a URL that can post into somebody's Slack is
//! not that, so it comes from the flag or the environment. And it does not
//! alert twice about the same day: an alert that repeats every hour trains
//! people to ignore it, which is worse than not sending it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{self, Config, Watch as Settings};
use crate::ga::{DateRange, Ga, ReportRequest};
use crate::mcp::unit_of;
use crate::render::{self, bold, dim, paint, panel_bottom, panel_top};
use crate::report::Format;
use crate::theme::{glyph, ore, Kind, Metric, OVERVIEW};

/// Days the baseline averages over when nothing says otherwise. Four weeks
/// covers every day of the week the same number of times, so a site that is
/// quiet at weekends is not permanently half a standard deviation from its own
/// normal.
pub const BASELINE_DAYS: u32 = 28;

/// A baseline below this never fires a count metric. On a site averaging four
/// conversions a day, one quiet day is a 25% "drop" that means nothing.
const MIN_BASELINE: f64 = 10.0;

/// Floor on `--every`. GA4's daily numbers settle over hours, not seconds, so
/// a tighter loop spends quota to re-read a number that has not changed.
const MIN_INTERVAL: u64 = 60;

/// How long to give a webhook before giving up on it.
const POST_TIMEOUT: Duration = Duration::from_secs(15);

// ------------------------------------------------------------- thresholds ---

/// Default deviation per metric, in percent.
///
/// Not one number for all six: on a small property conversions swing by a
/// third on an ordinary Tuesday, while bounce rate barely moves, so a single
/// threshold either shouts about the first or never notices the second.
fn default_threshold(api: &str) -> f64 {
    match api {
        "keyEvents" => 40.0,
        "bounceRate" => 20.0,
        "averageSessionDuration" => 25.0,
        _ => 30.0,
    }
}

/// The config key a metric answers to, alongside its GA4 API name. Short and
/// snake_case because it is something a person types into TOML — `page views`
/// would need quoting, and `screenPageViews` is not what anybody calls it.
fn key_of(api: &'static str) -> &'static str {
    match api {
        "totalUsers" => "users",
        "sessions" => "sessions",
        "screenPageViews" => "views",
        "keyEvents" => "conversions",
        "bounceRate" => "bounce_rate",
        "averageSessionDuration" => "avg_session",
        other => other,
    }
}

/// The thresholds and window in force for a property: its own settings where
/// it named any, the defaults everywhere else.
struct Rules<'a> {
    settings: Option<&'a Settings>,
    baseline_days: u32,
}

impl<'a> Rules<'a> {
    fn new(settings: Option<&'a Settings>, flag: Option<u32>) -> Rules<'a> {
        // The flag wins, then the property's setting, then the default — the
        // same precedence every other setting in this CLI follows.
        let baseline_days = flag
            .or_else(|| settings.and_then(|s| s.baseline_days))
            .unwrap_or(BASELINE_DAYS)
            .max(2);
        Rules {
            settings,
            baseline_days,
        }
    }

    fn threshold(&self, metric: &Metric) -> f64 {
        self.settings
            .and_then(|s| s.threshold(key_of(metric.api), metric.api))
            .unwrap_or_else(|| default_threshold(metric.api))
    }

    fn min_baseline(&self) -> f64 {
        self.settings
            .and_then(|s| s.min_baseline)
            .unwrap_or(MIN_BASELINE)
    }
}

// ----------------------------------------------------------------- alerts ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Nothing recorded at all, where there is normally something. A tag
    /// removed in a deploy, a broken consent banner, or a site that is down —
    /// the one alert here that is almost never a false positive.
    Silent,
    Drop,
    Spike,
}

impl Trigger {
    fn slug(self) -> &'static str {
        match self {
            Trigger::Silent => "silent",
            Trigger::Drop => "drop",
            Trigger::Spike => "spike",
        }
    }

    fn glyph(self) -> char {
        match self {
            Trigger::Silent => '·',
            Trigger::Drop => glyph::DOWN,
            Trigger::Spike => glyph::UP,
        }
    }

    fn color(self) -> ratatui::style::Color {
        match self {
            // Silence is the serious one, so it gets the alarming color even
            // though a spike is the larger number.
            Trigger::Silent | Trigger::Drop => ore::redstone(),
            Trigger::Spike => ore::gold(),
        }
    }
}

pub struct Alert {
    metric: &'static Metric,
    latest: f64,
    baseline: f64,
    /// Deviation from the baseline, in percent. Zero for a silent metric,
    /// where the interesting number is the baseline it stopped matching.
    change_pct: f64,
    threshold_pct: f64,
    trigger: Trigger,
}

impl Alert {
    /// The dedupe key: metric and direction, so a drop that becomes a spike is
    /// news again but the same drop is not.
    fn key(&self) -> String {
        format!("{}:{}", key_of(self.metric.api), self.trigger.slug())
    }
}

/// One pass over one property.
pub struct Findings {
    property: String,
    title: String,
    /// The day checked, as GA dated it. `None` only when the window came back
    /// completely empty, which is its own kind of answer.
    date: Option<String>,
    baseline_days: u32,
    /// Nothing anywhere in the window — a property that is not collecting, or
    /// an id that is not the one the site reports to. Six "silent" alerts
    /// would be six ways of saying this once.
    dark: bool,
    alerts: Vec<Alert>,
}

impl Findings {
    pub fn quiet(&self) -> bool {
        self.alerts.is_empty() && !self.dark
    }

    /// Drop the alerts already reported for this same day.
    fn less(mut self, already: &[String]) -> Findings {
        self.alerts.retain(|a| !already.contains(&a.key()));
        self
    }
}

// ------------------------------------------------------------------ check ---

/// Compare one metric against its baseline.
fn check(metric: &'static Metric, latest: f64, baseline: f64, rules: &Rules) -> Option<Alert> {
    // Nothing to be unusual against. A property in its first week is not an
    // anomaly, and dividing by it would be a percentage of nothing.
    if baseline <= 0.0 {
        return None;
    }

    // The floor is a count of things, so it only means anything for a count.
    // Applying it to a bounce rate would compare 0.42 against 10 and silence
    // the metric permanently.
    let floored = metric.kind == Kind::Count && baseline < rules.min_baseline();
    if floored {
        return None;
    }

    let threshold_pct = rules.threshold(metric);

    // A count that went to nothing is reported as silence rather than as a
    // 100% drop: the number is the same, but what it means — and what you do
    // about it — is not.
    if latest == 0.0 && metric.kind == Kind::Count {
        return Some(Alert {
            metric,
            latest,
            baseline,
            change_pct: 0.0,
            threshold_pct,
            trigger: Trigger::Silent,
        });
    }

    let change_pct = (latest - baseline) / baseline * 100.0;
    if !change_pct.is_finite() || change_pct.abs() < threshold_pct {
        return None;
    }

    Some(Alert {
        metric,
        latest,
        baseline,
        change_pct,
        threshold_pct,
        trigger: if change_pct > 0.0 {
            Trigger::Spike
        } else {
            Trigger::Drop
        },
    })
}

/// The baseline for one metric over the window.
///
/// The denominator is the argument. For a count, a day GA returned no row for
/// is a day the site had none, and it belongs in the divisor — a site that
/// went dark for a week has a genuinely lower daily average, not the same one
/// measured over fewer days. For a rate or an average it does not: a day with
/// no sessions has no bounce rate to average in, and counting it as zero would
/// pull the baseline toward a number the site never had.
fn baseline_of(kind: Kind, sum: f64, days_with_data: usize, window_days: u32) -> f64 {
    match kind {
        Kind::Count => sum / window_days.max(1) as f64,
        Kind::Ratio | Kind::Duration => match days_with_data {
            0 => 0.0,
            n => sum / n as f64,
        },
    }
}

/// Fetch and check one property.
async fn examine(
    ga: &Ga,
    property: &str,
    title: &str,
    settings: Option<&Settings>,
    baseline_flag: Option<u32>,
) -> Result<Findings> {
    let rules = Rules::new(settings, baseline_flag);
    let metrics: Vec<&str> = OVERVIEW.iter().map(|m| m.api).collect();
    let days = rules.baseline_days;

    // Two reports rather than one window split in memory, because a day with
    // no traffic comes back as no row at all. Asking for yesterday on its own
    // makes an empty answer unambiguous: it is the silence, not a row that
    // slid out of the window.
    let (latest, history) = tokio::try_join!(
        ga.report(
            property,
            ReportRequest::new(&metrics)
                .by(&["date"])
                .range(DateRange::yesterday())
        ),
        ga.report(
            property,
            ReportRequest::new(&metrics)
                .by(&["date"])
                .range(DateRange::span(days + 1, 2))
        )
    )?;

    // The date comes from GA, never from this machine's clock: `yesterday`
    // resolves in the property's timezone, which may not be this one.
    let date = latest
        .rows
        .first()
        .map(|row| crate::mcp::iso_date(row.dimension(0)))
        .or_else(|| day_after(history.rows.last()?.dimension(0)));

    if latest.rows.is_empty() && history.rows.is_empty() {
        return Ok(Findings {
            property: property.to_string(),
            title: title.to_string(),
            date,
            baseline_days: days,
            dark: true,
            alerts: Vec::new(),
        });
    }

    let alerts = OVERVIEW
        .iter()
        .enumerate()
        .filter_map(|(i, metric)| {
            let today = latest.rows.first().map(|row| row.metric(i)).unwrap_or(0.0);
            let sum: f64 = history.rows.iter().map(|row| row.metric(i)).sum();
            let base = baseline_of(metric.kind, sum, history.rows.len(), days);
            check(metric, today, base, &rules)
        })
        .collect();

    Ok(Findings {
        property: property.to_string(),
        title: title.to_string(),
        date,
        baseline_days: days,
        dark: false,
        alerts,
    })
}

/// `YYYYMMDD` plus one day, as `YYYY-MM-DD`. Used to name the day that
/// reported nothing, from the last day that reported something.
fn day_after(raw: &str) -> Option<String> {
    let date = chrono::NaiveDate::parse_from_str(raw, "%Y%m%d").ok()?;
    Some(date.succ_opt()?.format("%Y-%m-%d").to_string())
}

// -------------------------------------------------------------- rendering ---

/// Panels, for a person reading the output of a cron job in their mail.
fn panels(findings: &Findings) -> String {
    let mut out = format!(
        "\n{}\n\n",
        panel_top(&format!(
            "WATCH · {} · {}",
            findings.title.to_uppercase(),
            findings.date.as_deref().unwrap_or("no dated data")
        ))
    );

    if findings.dark {
        out.push_str(&format!(
            "  {}\n  {}\n\n",
            paint("nothing recorded in the whole window.", ore::redstone()),
            dim(&format!(
                "{} days without a single row — check the tag is installed, \
                 or that {} is the right property.",
                findings.baseline_days, findings.property
            ))
        ));
        out.push_str(&format!("{}\n", panel_bottom()));
        return out;
    }

    if findings.alerts.is_empty() {
        out.push_str(&format!(
            "  {}\n\n",
            dim(&format!(
                "nothing to report — all {} metrics within their {}-day normal.",
                OVERVIEW.len(),
                findings.baseline_days
            ))
        ));
        out.push_str(&format!("{}\n", panel_bottom()));
        return out;
    }

    for alert in &findings.alerts {
        let headline = match alert.trigger {
            Trigger::Silent => paint("nothing recorded", alert.trigger.color()),
            _ => paint(
                &format!("{}{:.0}%", alert.trigger.glyph(), alert.change_pct.abs()),
                alert.trigger.color(),
            ),
        };
        out.push_str(&format!(
            "  {} {}  {}  {}\n",
            paint(&alert.trigger.glyph().to_string(), alert.trigger.color()),
            bold(&paint(alert.metric.craft, (alert.metric.color)())),
            bold(&render::value(alert.metric, alert.latest)),
            headline,
        ));
        out.push_str(&format!(
            "    {}\n\n",
            dim(&format!(
                "{}-day normal {} · fires past {:.0}%",
                findings.baseline_days,
                render::value(alert.metric, alert.baseline),
                alert.threshold_pct,
            ))
        ));
    }

    out.push_str(&format!("{}\n", panel_bottom()));
    out
}

/// One object, the same way `overview --format json` answers.
fn as_json(findings: &Findings) -> Value {
    json!({
        "property": findings.property,
        "title": findings.title,
        "date": findings.date,
        "baseline_days": findings.baseline_days,
        "quiet": findings.quiet(),
        "no_data": findings.dark,
        "alerts": findings.alerts.iter().map(|a| json!({
            "metric": a.metric.api,
            "label": a.metric.plain,
            "unit": unit_of(a.metric.kind),
            "trigger": a.trigger.slug(),
            "value": a.latest,
            "baseline": a.baseline,
            "change_pct": a.change_pct,
            "threshold_pct": a.threshold_pct,
        })).collect::<Vec<_>>(),
    })
}

/// A Block Kit payload, ready for an incoming webhook.
fn as_slack(findings: &Findings) -> Value {
    let heading = if findings.dark {
        format!("⛏ {} · nothing recorded", findings.title)
    } else {
        format!(
            "⛏ {} · {} alert{}",
            findings.title,
            findings.alerts.len(),
            if findings.alerts.len() == 1 { "" } else { "s" }
        )
    };

    let mut blocks = vec![json!({
        "type": "header",
        "text": { "type": "plain_text", "text": truncate(&heading, 150) },
    })];

    if findings.dark {
        blocks.push(json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": format!(
                "*No rows at all in {} days.* Check the tag is installed, or that `{}` \
                 is the property the site reports to.",
                findings.baseline_days, findings.property,
            )},
        }));
    }

    for alert in &findings.alerts {
        let line = match alert.trigger {
            Trigger::Silent => format!(
                "*{}* — nothing recorded, against a {}-day normal of {}",
                alert.metric.plain,
                findings.baseline_days,
                render::value(alert.metric, alert.baseline),
            ),
            _ => format!(
                "*{}* {} — {} against a {}-day normal of {}",
                alert.metric.plain,
                render::value(alert.metric, alert.latest),
                format_args!("{}{:.0}%", alert.trigger.glyph(), alert.change_pct.abs()),
                findings.baseline_days,
                render::value(alert.metric, alert.baseline),
            ),
        };
        blocks.push(json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": line },
        }));
    }

    if let Some(date) = &findings.date {
        blocks.push(json!({
            "type": "context",
            "elements": [{
                "type": "mrkdwn",
                "text": format!("{} · property {}", date, findings.property),
            }],
        }));
    }

    json!({ "blocks": blocks })
}

/// Character-wise, so a multi-byte title is not sliced mid-codepoint.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

// ------------------------------------------------------------------ state ---

/// What has already been said, so it is not said again.
///
/// Keyed by property, then by the day being reported on: when GA rolls over to
/// a new day the slate is clean, which is what makes `--every 3600` send one
/// alert about a drop rather than twenty-four.
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(flatten)]
    properties: BTreeMap<String, Said>,
}

#[derive(Default, Serialize, Deserialize)]
struct Said {
    date: String,
    keys: Vec<String>,
}

impl State {
    fn path() -> Result<PathBuf> {
        Ok(config::home()?.join("watch.json"))
    }

    /// A missing or unreadable state file is an empty one. The cost of getting
    /// this wrong is one duplicate alert; the cost of failing here is a watch
    /// that stops watching.
    fn load() -> State {
        let Ok(path) = Self::path() else {
            return State::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// What has already been reported for this property on this day.
    fn said(&self, property: &str, date: &str) -> &[String] {
        match self.properties.get(property) {
            Some(said) if said.date == date => &said.keys,
            _ => &[],
        }
    }

    fn record(&mut self, property: &str, date: &str, keys: impl Iterator<Item = String>) {
        let said = self.properties.entry(property.to_string()).or_default();
        if said.date != date {
            said.date = date.to_string();
            said.keys.clear();
        }
        said.keys.extend(keys);
    }

    fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))
    }
}

// --------------------------------------------------------------- delivery ---

/// POST the payload and insist on a 2xx.
///
/// Slack answers a bad payload with 200 and the word `invalid`, so the status
/// alone is not the whole check.
pub(crate) async fn post(webhook: &str, payload: &Value) -> Result<()> {
    let response = reqwest::Client::builder()
        .timeout(POST_TIMEOUT)
        .build()?
        .post(webhook)
        .json(payload)
        .send()
        .await
        .context("could not reach the webhook")?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("the webhook answered {status}: {}", body.trim());
    }
    if body.trim().eq_ignore_ascii_case("invalid_payload") {
        anyhow::bail!("the webhook rejected the payload as invalid");
    }
    Ok(())
}

// ------------------------------------------------------------------ entry ---

pub struct Options {
    pub baseline: Option<u32>,
    pub every: Option<u64>,
    pub webhook: Option<String>,
    pub format: Format,
    pub demo: bool,
}

/// Check once and exit, or keep checking.
pub async fn run(cfg: &Config, property: Option<&str>, opts: Options) -> Result<()> {
    if opts.demo {
        // The shop window, same as everywhere else: no account, no
        // subscription, so what an alert looks like can be seen before
        // anything is paid for or wired up.
        let findings = demo_findings(opts.baseline.unwrap_or(BASELINE_DAYS));
        emit(&findings, opts.format, opts.webhook.as_deref()).await?;
        return finish(&findings);
    }

    // Ask Supabase where the subscription stands, the same way the dashboard
    // does on the way in, then gate on the answer.
    let supporter = crate::license::sync(cfg.supporter).await;
    let cfg = &Config::load().unwrap_or_default();
    crate::license::gate(supporter, "craft watch").map_err(|reason| anyhow::anyhow!(reason))?;

    let id = cfg.resolve_property(property)?;
    let title = cfg
        .find(&id)
        .map(|p| p.display())
        .unwrap_or_else(|| format!("property {id}"));
    let ga = Ga::new()?;

    let Some(every) = opts.every else {
        let findings = pass(&ga, cfg, &id, &title, &opts).await?;
        return finish(&findings);
    };

    // A daemon. A failed pass is reported and slept off rather than fatal:
    // the network drops, GA rate-limits, a laptop suspends, and a watch that
    // exits on the first of those is a watch that was not running when the
    // thing it was watching for happened.
    let every = every.max(MIN_INTERVAL);
    loop {
        match pass(&ga, cfg, &id, &title, &opts).await {
            Ok(_) => {}
            Err(err) => eprintln!(
                "  {} {}",
                paint("⛏", ore::redstone()),
                dim(&format!("check failed, retrying in {every}s: {err}"))
            ),
        }
        tokio::time::sleep(Duration::from_secs(every)).await;
    }
}

/// One check: fetch, suppress what has already been said, deliver, remember.
async fn pass(ga: &Ga, cfg: &Config, id: &str, title: &str, opts: &Options) -> Result<Findings> {
    let findings = examine(ga, id, title, cfg.watch_for(id), opts.baseline).await?;

    let mut state = State::load();
    let date = findings.date.clone().unwrap_or_default();
    let findings = findings.less(state.said(id, &date));

    emit(&findings, opts.format, opts.webhook.as_deref()).await?;

    // Recorded only after delivery: a webhook that was unreachable has not
    // told anybody anything, and the next pass should try again rather than
    // treat the alert as spent.
    if !findings.alerts.is_empty() {
        state.record(id, &date, findings.alerts.iter().map(|a| a.key()));
        state.save()?;
    }
    Ok(findings)
}

/// Print, and POST if there is somewhere to POST to.
async fn emit(findings: &Findings, format: Format, webhook: Option<&str>) -> Result<()> {
    match format {
        Format::Panels => print!("{}", panels(findings)),
        Format::Json => println!("{}", as_json(findings)),
        // A quiet hour is not worth a Slack message, and an empty body is not
        // a payload a webhook accepts. So slack prints only when there is
        // something to say — which is also what makes `craft watch --format
        // slack | curl -d @-` in cron behave, since curl is never handed an
        // empty document.
        Format::Slack => {
            if !findings.quiet() {
                println!("{}", as_slack(findings));
            }
        }
    }

    if let Some(webhook) = webhook {
        if !findings.quiet() {
            post(webhook, &payload(findings, format, Some(webhook))).await?;
        }
    }
    Ok(())
}

/// What a webhook receives.
///
/// `--format` chooses it, with two exceptions.
///
/// Panels, because they have no wire form — a payload of ANSI escapes is not a
/// payload. Somebody who left `--format` alone and passed a webhook means "put
/// this in my chat".
///
/// And Slack, because it cannot read anything but its own shape: handed a bare
/// JSON object it answers `400 no_text`. `--format json` with a Slack
/// destination is not a request worth honouring literally — it is a report
/// printed as JSON, which is what the caller asked for, going to a place that
/// only speaks blocks. The destination wins over the flag there, which is what
/// keeps `craft slack --install` from turning `--format json` into an error:
/// that destination is saved, not typed, so the flag was never about it.
fn payload(findings: &Findings, format: Format, webhook: Option<&str>) -> Value {
    if webhook.is_some_and(is_slack) {
        return as_slack(findings);
    }
    match format {
        Format::Json => as_json(findings),
        Format::Slack | Format::Panels => as_slack(findings),
    }
}

/// Whether a URL is one only Slack will answer.
///
/// The host, not where the URL came from: this has to cover the webhook
/// `craft slack --install` saved and one somebody pasted into `--webhook`
/// themselves, and both are the same string from the same place.
fn is_slack(webhook: &str) -> bool {
    webhook.starts_with("https://hooks.slack.com/")
}

/// Exit 2 when something fired, the way a monitoring command is expected to.
///
/// `process::exit` runs no destructors, and stdout is block-buffered when it
/// is a pipe — so the flush is not optional, it is the difference between a
/// piped alert arriving and vanishing.
fn finish(findings: &Findings) -> Result<()> {
    if findings.quiet() {
        return Ok(());
    }
    std::io::stdout().flush().ok();
    std::process::exit(2);
}

// ------------------------------------------------------------------- demo ---

/// A drop, a silence and a spike, so `--demo` shows all three shapes at once.
fn demo_findings(baseline_days: u32) -> Findings {
    let at = |i: usize| OVERVIEW[i];
    Findings {
        property: "397412345".to_string(),
        title: "Contoso Labs (demo)".to_string(),
        date: Some("2026-09-02".to_string()),
        baseline_days,
        dark: false,
        alerts: vec![
            Alert {
                metric: at(0),
                latest: 412.0,
                baseline: 664.0,
                change_pct: -37.95,
                threshold_pct: 30.0,
                trigger: Trigger::Drop,
            },
            Alert {
                metric: at(3),
                latest: 0.0,
                baseline: 12.4,
                change_pct: 0.0,
                threshold_pct: 40.0,
                trigger: Trigger::Silent,
            },
            Alert {
                metric: at(4),
                latest: 0.71,
                baseline: 0.44,
                change_pct: 61.4,
                threshold_pct: 20.0,
                trigger: Trigger::Spike,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{CREEPER_RATE, DIAMONDS, VILLAGERS};

    fn rules() -> Rules<'static> {
        Rules::new(None, None)
    }

    #[test]
    fn a_drop_past_the_threshold_fires_and_one_inside_it_does_not() {
        let alert = check(&VILLAGERS, 412.0, 664.0, &rules()).expect("38% is past 30%");
        assert_eq!(alert.trigger, Trigger::Drop);
        assert!((alert.change_pct - -37.95).abs() < 0.01);

        assert!(check(&VILLAGERS, 600.0, 664.0, &rules()).is_none(), "10%");
    }

    #[test]
    fn a_count_that_went_to_nothing_is_silence_not_a_hundred_percent_drop() {
        let alert = check(&DIAMONDS, 0.0, 12.4, &rules()).expect("zero against a baseline");
        assert_eq!(alert.trigger, Trigger::Silent);
        // The drop is implied; the useful number is what it stopped matching.
        assert_eq!(alert.change_pct, 0.0);
        assert_eq!(alert.key(), "conversions:silent");
    }

    #[test]
    fn a_property_with_no_history_is_not_an_anomaly() {
        assert!(check(&VILLAGERS, 900.0, 0.0, &rules()).is_none());
    }

    #[test]
    fn the_floor_keeps_a_tiny_baseline_from_firing_on_noise() {
        // Four a day, none yesterday: a 100% "drop" that means nothing.
        assert!(check(&DIAMONDS, 0.0, 4.0, &rules()).is_none());
        // The same shape once the site is big enough for it to be signal.
        assert!(check(&DIAMONDS, 0.0, 40.0, &rules()).is_some());
    }

    #[test]
    fn the_floor_does_not_apply_to_a_rate() {
        // A bounce rate baseline of 0.44 is below the count floor of 10, and
        // silencing it for that reason would silence it forever.
        let alert = check(&CREEPER_RATE, 0.71, 0.44, &rules()).expect("61% is past 20%");
        assert_eq!(alert.trigger, Trigger::Spike);
    }

    #[test]
    fn a_rate_is_never_reported_as_silence() {
        // Zero bounce rate is odd, but it is not the same event as a count
        // going to nothing, and Silent's promise is that it means tracking.
        match check(&CREEPER_RATE, 0.0, 0.44, &rules()) {
            Some(alert) => assert_eq!(alert.trigger, Trigger::Drop),
            None => panic!("a 100% move should still fire"),
        }
    }

    #[test]
    fn a_count_baseline_divides_by_the_window_a_rate_by_the_days_it_had() {
        // Seven days of history, only three of which reported.
        assert_eq!(baseline_of(Kind::Count, 210.0, 3, 7), 30.0);
        assert_eq!(baseline_of(Kind::Ratio, 1.5, 3, 7), 0.5);
        assert_eq!(baseline_of(Kind::Duration, 0.0, 0, 7), 0.0);
    }

    #[test]
    fn config_thresholds_beat_the_defaults_and_the_flag_beats_both() {
        let mut thresholds = BTreeMap::new();
        thresholds.insert("users".to_string(), 5.0);
        let settings = Settings {
            baseline_days: Some(14),
            min_baseline: None,
            thresholds,
        };

        let rules = Rules::new(Some(&settings), None);
        assert_eq!(rules.baseline_days, 14);
        assert_eq!(rules.threshold(&VILLAGERS), 5.0);
        // Not named in the config, so still its own default.
        assert_eq!(rules.threshold(&DIAMONDS), 40.0);

        assert_eq!(Rules::new(Some(&settings), Some(90)).baseline_days, 90);
    }

    #[test]
    fn a_baseline_of_one_day_is_raised_to_something_comparable() {
        // `span(days + 1, 2)` with days = 1 would be a window with no days in
        // it, and a baseline of nothing fires on everything.
        assert_eq!(Rules::new(None, Some(0)).baseline_days, 2);
    }

    #[test]
    fn the_same_alert_is_said_once_a_day() {
        let mut state = State::default();
        assert!(state.said("1", "2026-09-02").is_empty());

        state.record("1", "2026-09-02", ["users:drop".to_string()].into_iter());
        assert_eq!(state.said("1", "2026-09-02"), ["users:drop"]);
        // A new day is news again.
        assert!(state.said("1", "2026-09-03").is_empty());
    }

    #[test]
    fn suppression_drops_only_what_was_already_said() {
        let findings = demo_findings(28).less(&["conversions:silent".to_string()]);
        let keys: Vec<String> = findings.alerts.iter().map(|a| a.key()).collect();
        assert_eq!(keys, ["users:drop", "bounce_rate:spike"]);
    }

    #[test]
    fn a_dark_window_reports_once_rather_than_six_times() {
        let findings = Findings {
            property: "1".into(),
            title: "t".into(),
            date: None,
            baseline_days: 28,
            dark: true,
            alerts: Vec::new(),
        };
        assert!(!findings.quiet(), "no data is not nothing to report");
        assert_eq!(as_json(&findings)["no_data"], true);
        assert_eq!(as_json(&findings)["alerts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn slack_stays_silent_on_a_quiet_day() {
        let quiet = Findings {
            property: "1".into(),
            title: "t".into(),
            date: Some("2026-09-02".into()),
            baseline_days: 28,
            dark: false,
            alerts: Vec::new(),
        };
        assert!(quiet.quiet());
        // The payload is only built when there is something to say, but it
        // must be well-formed when there is.
        let loud = as_slack(&demo_findings(28));
        assert_eq!(loud["blocks"][0]["type"], "header");
        assert_eq!(
            loud["blocks"][0]["text"]["text"],
            "⛏ Contoso Labs (demo) · 3 alerts"
        );
    }

    #[test]
    fn a_webhook_gets_the_shape_the_format_asked_for() {
        let findings = demo_findings(28);

        // The bug this covers: --format json --webhook used to print JSON and
        // post Block Kit, so the flag was ignored for the one destination the
        // caller had actually named.
        let own_service = Some("https://example.com/hook");
        let json = payload(&findings, Format::Json, own_service);
        assert!(json["alerts"].is_array(), "not the json shape: {json}");
        assert!(json.get("blocks").is_none());

        for chat in [Format::Slack, Format::Panels] {
            let blocks = payload(&findings, chat, own_service);
            assert!(blocks["blocks"].is_array(), "not the chat shape: {blocks}");
        }
    }

    #[test]
    fn slack_gets_blocks_whatever_the_format_says() {
        let findings = demo_findings(28);
        let slack = Some("https://hooks.slack.com/services/T/B/x");

        // v0.10.0's bug, and it needed no flags to hit: with a destination
        // saved by `craft slack --install`, plain `craft watch --format json`
        // posted a bare JSON object to Slack, which answers `400 no_text`.
        for format in [Format::Json, Format::Slack, Format::Panels] {
            let body = payload(&findings, format, slack);
            assert!(body["blocks"].is_array(), "slack cannot read this: {body}");
        }
    }

    #[test]
    fn only_a_real_slack_host_is_treated_as_slack() {
        assert!(is_slack("https://hooks.slack.com/services/T/B/x"));
        // A lookalike host must not silently reshape somebody's payload.
        assert!(!is_slack("https://hooks.slack.com.evil.test/x"));
        assert!(!is_slack("http://hooks.slack.com/services/T/B/x"));
        assert!(!is_slack("https://example.com/hook"));
    }

    #[test]
    fn the_day_after_a_ga_date_is_the_day_that_went_quiet() {
        assert_eq!(day_after("20260902").as_deref(), Some("2026-09-03"));
        // Month and year rollovers, which is the whole reason not to do this
        // with arithmetic on the string.
        assert_eq!(day_after("20260831").as_deref(), Some("2026-09-01"));
        assert_eq!(day_after("20261231").as_deref(), Some("2027-01-01"));
        assert_eq!(day_after("nonsense"), None);
    }

    #[test]
    fn json_names_the_property_the_day_and_the_threshold_that_fired() {
        let payload = as_json(&demo_findings(28));
        assert_eq!(payload["quiet"], false);
        assert_eq!(payload["baseline_days"], 28);
        assert_eq!(payload["date"], "2026-09-02");
        assert_eq!(payload["alerts"][0]["metric"], "totalUsers");
        assert_eq!(payload["alerts"][0]["trigger"], "drop");
        assert_eq!(payload["alerts"][0]["threshold_pct"], 30.0);
        assert_eq!(payload["alerts"][1]["trigger"], "silent");
        assert_eq!(payload["alerts"][1]["unit"], "count");
    }
}
