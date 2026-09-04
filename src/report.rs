//! Machine-readable renderings of `craft overview`.
//!
//! The panels are for a person looking at a terminal. These two are for
//! everything else: a script that wants the numbers, and a cron line that
//! wants them posted somewhere a team will see them.
//!
//! Both go through `mcp::status_payload`, the same function the `site_status`
//! tool answers with. That is deliberate — the number an assistant reads over
//! MCP, the number a script parses out of a pipe, and the number on the panel
//! are one number computed once, so the three cannot drift into disagreeing
//! about the same week.

use serde_json::{json, Value};

use crate::mcp::{change_pct, iso_date, status_payload};
use crate::render;
use crate::theme::{glyph, OVERVIEW};

/// How `craft overview` should render.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Ore-textured panels, for a person at a terminal.
    Panels,
    /// One JSON object — the shape `site_status` answers with over MCP.
    Json,
    /// A Slack Block Kit payload, ready to POST to an incoming webhook.
    Slack,
}

/// Everything both renderers need, gathered once by `cmd_overview` so neither
/// of them issues a query of its own.
pub struct Overview<'a> {
    pub property: &'a str,
    pub title: &'a str,
    pub days: u32,
    /// Totals for `theme::OVERVIEW`, in that order.
    pub totals: &'a [f64],
    /// The same metrics over the period immediately before.
    pub prior: &'a [f64],
    /// Daily users as `(YYYYMMDD, users)`, chronological.
    pub daily: &'a [(String, f64)],
    pub empty: bool,
}

impl Overview<'_> {
    fn series(&self) -> Vec<f64> {
        self.daily.iter().map(|(_, users)| *users).collect()
    }

    /// The window the numbers actually cover, read off the returned rows
    /// rather than computed from the local clock: `last_days` is relative and
    /// resolves in the property's timezone, which is not necessarily this
    /// machine's. `None` when no rows came back to read it from.
    fn window(&self) -> Option<(String, String)> {
        let first = self.daily.first()?;
        let last = self.daily.last()?;
        Some((iso_date(&first.0), iso_date(&last.0)))
    }
}

/// One JSON object on stdout.
pub fn json(overview: &Overview) -> Value {
    let mut payload = status_payload(
        overview.totals,
        overview.prior,
        overview.daily,
        overview.empty,
    );

    // `status_payload` answers the question "how is the site doing"; a caller
    // reading this off a pipe also needs to know which site and when, which
    // over MCP is carried by the tool call instead.
    payload["property"] = json!(overview.property);
    payload["title"] = json!(overview.title);
    payload["days"] = json!(overview.days);
    if let Some((start, end)) = overview.window() {
        payload["window"] = json!({ "start": start, "end": end });
    }

    payload
}

/// A Block Kit message. Shaped for an incoming webhook, which takes the same
/// `{"blocks": [...]}` envelope as `chat.postMessage`.
pub fn slack(overview: &Overview) -> Value {
    let mut blocks = vec![json!({
        "type": "header",
        "text": {
            "type": "plain_text",
            // Block Kit caps a header at 150 characters and truncates past it.
            "text": truncate(&format!("{} · last {} days", overview.title, overview.days), 150),
        },
    })];

    if overview.empty {
        blocks.push(json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": "_no data in this window._" },
        }));
        return json!({ "blocks": blocks });
    }

    // Two columns of three. A section takes at most ten fields and OVERVIEW is
    // six, so the headline metrics always fit in one block.
    let fields: Vec<Value> = OVERVIEW
        .iter()
        .enumerate()
        .map(|(i, metric)| {
            let now = overview.totals.get(i).copied().unwrap_or(0.0);
            let before = overview.prior.get(i).copied().unwrap_or(0.0);
            json!({
                "type": "mrkdwn",
                "text": format!(
                    "*{}*\n{}  {}",
                    metric.plain,
                    render::value(metric, now),
                    delta(now, before),
                ),
            })
        })
        .collect();
    blocks.push(json!({ "type": "section", "fields": fields }));

    let series = overview.series();
    if series.len() > 1 {
        // The block glyphs survive the trip: Slack renders them as text, and
        // in a proportional font a sparkline of them still reads as a shape.
        blocks.push(json!({
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!("daily users  `{}`", render::spark_glyphs(&series)),
            },
        }));
    }

    for achievement in crate::achievements::unlocked(&snapshot(overview))
        .iter()
        .take(3)
    {
        blocks.push(json!({
            "type": "context",
            "elements": [{
                "type": "mrkdwn",
                "text": format!("*{}* · {}", achievement.title, achievement.detail),
            }],
        }));
    }

    if let Some((start, end)) = overview.window() {
        blocks.push(json!({
            "type": "context",
            "elements": [{
                "type": "mrkdwn",
                "text": format!("{} → {} · property {}", start, end, overview.property),
            }],
        }));
    }

    json!({ "blocks": blocks })
}

/// Period-over-period change as plain text.
///
/// Mirrors `render::delta`'s thresholds — no baseline says nothing, under half
/// a percent is flat — so a Slack digest and the panel it came from never
/// disagree about whether the week moved. What it drops is the coloring, and
/// with it the `lower_is_better` argument: an arrow states the direction, which
/// is true of bounce rate as much as of users, and Block Kit has no red to
/// tint it with anyway.
fn delta(now: f64, before: f64) -> String {
    let Some(change) = change_pct(now, before) else {
        return String::new();
    };
    if !change.is_finite() {
        return String::new();
    }
    if change.abs() < 0.5 {
        return "— flat".to_string();
    }
    let arrow = if change > 0.0 { glyph::UP } else { glyph::DOWN };
    format!("{arrow}{:.0}%", change.abs())
}

/// The achievement inputs, in `OVERVIEW` order. Kept next to the renderer that
/// needs it rather than in `achievements`, which should not have to know how
/// the CLI happens to lay its totals out.
fn snapshot(overview: &Overview) -> crate::achievements::Snapshot {
    let at = |row: &[f64], i: usize| row.get(i).copied().unwrap_or(0.0);
    crate::achievements::Snapshot {
        users: at(overview.totals, 0),
        prev_users: at(overview.prior, 0),
        sessions: at(overview.totals, 1),
        views: at(overview.totals, 2),
        conversions: at(overview.totals, 3),
        prev_conversions: at(overview.prior, 3),
        bounce_rate: at(overview.totals, 4),
        prev_bounce_rate: at(overview.prior, 4),
        avg_duration: at(overview.totals, 5),
        daily_users: overview.series(),
    }
}

/// Character-wise, not byte-wise: a Block Kit limit is in characters, and
/// slicing a multi-byte title at a byte index panics.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Vec<f64>, Vec<f64>, Vec<(String, f64)>) {
        (
            vec![12_481.0, 18_203.0, 41_776.0, 312.0, 0.412, 214.0],
            vec![11_450.0, 17_004.0, 39_210.0, 258.0, 0.478, 191.0],
            vec![
                ("20260828".to_string(), 1402.0),
                ("20260829".to_string(), 1288.0),
                ("20260830".to_string(), 1531.0),
            ],
        )
    }

    fn overview<'a>(
        totals: &'a [f64],
        prior: &'a [f64],
        daily: &'a [(String, f64)],
        empty: bool,
    ) -> Overview<'a> {
        Overview {
            property: "397412345",
            title: "Contoso Labs",
            days: 7,
            totals,
            prior,
            daily,
            empty,
        }
    }

    #[test]
    fn json_carries_the_property_and_the_window_it_covers() {
        let (totals, prior, daily) = sample();
        let payload = json(&overview(&totals, &prior, &daily, false));

        assert_eq!(payload["property"], "397412345");
        assert_eq!(payload["days"], 7);
        assert_eq!(payload["window"]["start"], "2026-08-28");
        assert_eq!(payload["window"]["end"], "2026-08-30");
        // Straight from status_payload, so the MCP shape is still intact.
        assert_eq!(payload["metrics"][0]["metric"], "totalUsers");
        assert_eq!(payload["has_data"], true);
    }

    #[test]
    fn no_rows_means_no_window_rather_than_a_guessed_one() {
        let payload = json(&overview(&[], &[], &[], true));

        assert_eq!(payload["has_data"], false);
        assert!(payload.get("window").is_none());
    }

    #[test]
    fn slack_leads_with_a_header_and_fits_the_metrics_in_one_section() {
        let (totals, prior, daily) = sample();
        let payload = slack(&overview(&totals, &prior, &daily, false));
        let blocks = payload["blocks"].as_array().unwrap();

        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[0]["text"]["text"], "Contoso Labs · last 7 days");

        let fields = blocks[1]["fields"].as_array().unwrap();
        assert_eq!(fields.len(), OVERVIEW.len());
        assert!(fields.len() <= 10, "Block Kit caps a section at ten fields");
        assert_eq!(fields[0]["text"], "*users*\n12,481  ▲9%");
    }

    #[test]
    fn slack_says_so_rather_than_posting_empty_panels() {
        let payload = slack(&overview(&[], &[], &[], true));
        let blocks = payload["blocks"].as_array().unwrap();

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["text"]["text"], "_no data in this window._");
    }

    #[test]
    fn a_flat_week_reads_as_flat_and_a_first_week_says_nothing() {
        assert_eq!(delta(1000.0, 1000.0), "— flat");
        assert_eq!(delta(1002.0, 1000.0), "— flat");
        assert_eq!(delta(1100.0, 1000.0), "▲10%");
        assert_eq!(delta(900.0, 1000.0), "▼10%");
        // No baseline: growth from nothing is not a percentage.
        assert_eq!(delta(1000.0, 0.0), "");
    }

    #[test]
    fn a_long_title_is_cut_on_characters_not_bytes() {
        let wide = "⛏".repeat(200);
        let cut = truncate(&wide, 150);
        assert_eq!(cut.chars().count(), 150);
        assert!(cut.ends_with('…'));
    }
}
