//! The full-screen live dashboard (`anacraft dash`), built on ratatui.
//!
//! Everything on screen moves. Values ease toward their new targets instead of
//! snapping, bars keep a mining edge while they are still filling, the realtime
//! count is polled on its own faster cadence, and arrivals since the last poll
//! scroll past as a feed — so the panel changes between report refreshes rather
//! than sitting frozen for thirty seconds at a time.
//!
//! Network work runs in spawned tasks and lands over a channel; the frame loop
//! never awaits the API, which is what keeps the animation smooth across a slow
//! request.
//!
//! The one-shot commands render ANSI strings directly; those can't be reused
//! here because ratatui needs styled spans, so the block-drawing helpers are
//! reimplemented against `Line`/`Span`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::supports_keyboard_enhancement;
use rand::Rng;
use ratatui::prelude::*;
use ratatui::widgets::{Axis, Block, Borders, Chart, Clear, Dataset, GraphType, Paragraph};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::config::{Config, Property};
use crate::ga::{DateRange, Ga, ReportRequest};
use crate::render::{commas, value};
use crate::theme::{self, glyph, ore, OVERVIEW};

/// Frame budget. 20fps is plenty for block glyphs and leaves the CPU idle.
const FRAME: Duration = Duration::from_millis(50);
/// Default realtime polling cadence, deliberately independent of the report
/// refresh — this is the number people actually watch. Overridable with
/// `--live-refresh`; the floor keeps a fast setting from burning realtime
/// quota.
pub const LIVE_EVERY: u64 = 5;
const LIVE_FLOOR: u64 = 2;
/// How fast eased values close on their target. Higher is snappier; this lands
/// a big jump in a bit under a second.
const EASE: f64 = 4.5;
/// How long a row stays highlighted after its value changes.
const FLASH: Duration = Duration::from_millis(1400);
/// Live-feed entries older than this are dropped.
const FEED_TTL: Duration = Duration::from_secs(90);
/// The smallest terminal the dashboard will draw in.
///
/// Width comes from the vitals rows, which start clipping their deltas below
/// 80. Height is what the frame itself costs — a 3-row header, the supporter
/// box, the footer, the gutters between them, and the 10 rows the body is never
/// given less than. Under either, the panels don't degrade so much as
/// disintegrate, so the dashboard says what it needs and waits instead.
const MIN_COLS: u16 = 80;
const MIN_ROWS: u16 = 24;
/// The supporter box: one line and its borders.
const SUPPORTER_ROWS: u16 = 3;
/// Rows the event feed always occupies, lit or not.
///
/// A fixed field, because the panel is a screen: an LCD does not shrink when it
/// has less to say. It also stops the box below it walking up and down the
/// column every time an event ages out, which is the kind of motion a dashboard
/// left open on a second screen should never make.
const FEED_ROWS: usize = 6;
/// What the vitals panel needs: a label, a bar and a gap for each headline
/// metric, the daily sparkline, and its borders.
const VITALS_ROWS: u16 = (OVERVIEW.len() as u16) * 3 + 2;
/// The daily chart's box: three rows of bars, a caption, and borders.
const TREND_ROWS: u16 = 3 + 1 + 2;
/// What the live panel needs before it is worth drawing: the count, its
/// caption, the meter, the graph and the feed's full field, plus borders.
const LIVE_ROWS: u16 = 8 + FEED_ROWS as u16;
/// Two chunks and borders. It takes the column's spare rows on top of this.
const CHUNKS_ROWS: u16 = 6;
/// Ranked realms: eight country rows with bars, plus borders.
const REALMS_RANKED_ROWS: u16 = 10;
/// The map's box: nine rows of world, a caption, and borders.
const MAP_ROWS: u16 = 12;
/// The events chart: enough rows that the two lines are told apart, plus the
/// axis labels, the legend and the borders.
const EVENTS_ROWS: u16 = 14;
/// Width of the view-count column on the chunk rows.
const VIEWS_COLUMN: usize = 8;
/// Width of the share-of-page-views column beside it.
const SHARE_COLUMN: usize = 5;
/// Width of the "climbed two places" marker on a chunk's heading line.
const MOVED_COLUMN: usize = 4;
/// How often the live graph takes a column. Independent of the poll: the graph
/// scrolls on this clock whether or not a new sample has arrived, which is what
/// keeps the panel moving the way btop's graphs do.
const TRACE_EVERY: Duration = Duration::from_millis(500);
/// Columns of trace kept — two minutes at [`TRACE_EVERY`].
const HISTORY: usize = 240;

// ------------------------------------------------------------- animation ---

/// A scalar that chases its target instead of jumping to it.
#[derive(Clone, Copy)]
struct Eased {
    shown: f64,
    target: f64,
}

impl Eased {
    /// Starts at zero so the first frame after launch grows into place.
    fn new(target: f64) -> Eased {
        Eased { shown: 0.0, target }
    }

    fn to(&mut self, target: f64) {
        self.target = target;
    }

    /// Framerate-independent exponential ease-out: the per-frame step is
    /// derived from elapsed time, so a dropped frame doesn't slow the motion.
    fn step(&mut self, dt: f64) {
        if self.shown == self.target {
            return;
        }
        self.shown += (self.target - self.shown) * (1.0 - (-dt * EASE).exp());
        // Snap once the gap is invisible, otherwise `moving()` never settles
        // and the mining edge flickers forever.
        if (self.target - self.shown).abs() < (self.target.abs() * 1e-3).max(1e-4) {
            self.shown = self.target;
        }
    }

    fn moving(&self) -> bool {
        self.shown != self.target
    }
}

/// Fades from 1 to 0 over `FLASH` after a value changes.
fn flash_level(at: Option<Instant>) -> f64 {
    match at {
        Some(at) => {
            let elapsed = at.elapsed().as_secs_f64();
            let span = FLASH.as_secs_f64();
            if elapsed >= span {
                0.0
            } else {
                1.0 - elapsed / span
            }
        }
        None => 0.0,
    }
}

// ------------------------------------------------------------------ data ---

/// Event counts keyed by GA's `YYYYMMDD`, for the period and the one before it,
/// each already in chronological order.
type EventCounts = (Vec<(String, f64)>, Vec<(String, f64)>);

/// One report pass. The realtime number is not in here: it arrives on its own
/// cadence, and folding it in would make it as stale as the reports.
struct Snapshot {
    current: Vec<f64>,
    previous: Vec<f64>,
    daily: Vec<f64>,
    pages: Vec<(String, f64)>,
    realms: Vec<(String, f64)>,
    /// Event counts per day, for the period and the one before it.
    events: EventCounts,
}

/// A report arrives in pieces, and each piece is painted the moment it lands
/// rather than being held until the whole set is in — a slow chunk list no
/// longer delays the headline numbers.
enum Update {
    /// Totals and the period they are compared against travel together: a total
    /// without its previous period would render a wrong delta for a frame.
    Totals {
        current: Vec<f64>,
        previous: Vec<f64>,
    },
    Trend(Vec<f64>),
    Pages(Vec<(String, f64)>),
    /// Event counts per day, for the period and the one before it.
    Events(EventCounts),
    /// Users by country for the period — where they came from.
    Realms(Vec<(String, f64)>),
    Live {
        total: f64,
        realms: Vec<(String, f64)>,
    },
    Failed(String),
}

struct MetricRow {
    value: Eased,
    frac: Eased,
    previous: f64,
    flash: Option<Instant>,
}

struct PageRow {
    path: String,
    views: Eased,
    frac: Eased,
    /// Places climbed since the last report — `None` for a chunk that wasn't
    /// on the board before.
    moved: Option<i64>,
}

/// Events per day for the period and for the period before it.
///
/// The points are kept in the chart's own coordinates so the datasets can borrow
/// them straight out of the dashboard rather than being rebuilt every frame.
#[derive(Default)]
struct EventTrend {
    current: Vec<(f64, f64)>,
    /// The earlier period, laid over the same x range so the two read as a
    /// comparison rather than as one series twice as long.
    previous: Vec<(f64, f64)>,
    /// Day-of-month labels, one per point in `current`.
    days: Vec<String>,
    total: f64,
    total_previous: f64,
    peak: f64,
}

/// Which panels are on screen. Hidden panels give their rows and columns back
/// to whatever is left, so hiding one is a layout change rather than a blank
/// rectangle.
struct Panels {
    vitals: bool,
    live: bool,
    chunks: bool,
    realms_ranked: bool,
    trend: bool,
    map: bool,
    events: bool,
}

impl Panels {
    fn any(&self) -> bool {
        self.vitals
            || self.live
            || self.chunks
            || self.realms_ranked
            || self.trend
            || self.map
            || self.events
    }

    /// Panels that live in the right-hand column, top to bottom.
    fn right_any(&self) -> bool {
        self.live || self.chunks || self.realms_ranked || self.trend
    }
}

/// A change in the realtime count, shown in the live feed until it ages out.
struct FeedEvent {
    delta: f64,
    at: Instant,
}

struct Dash {
    title: String,
    days: u32,
    metrics: Vec<MetricRow>,
    daily: Vec<f64>,
    pages: Vec<PageRow>,
    /// Events per day, this period against the last.
    events: EventTrend,
    live: Eased,
    live_raw: f64,
    /// Realtime samples, oldest first — the live sparkline scrolls off this.
    history: VecDeque<f64>,
    /// Users by country over the period — where they came from, and the map's
    /// base layer.
    realms: Vec<(String, f64)>,
    /// Countries with somebody on the site right now, lit on top of the base.
    live_realms: Vec<(String, f64)>,
    feed: VecDeque<FeedEvent>,
    updated: String,
    /// Set when a refresh fails; the last good numbers stay on screen.
    error: Option<String>,
    /// Report parts still in the air. The spinner is on while this is non-zero.
    in_flight: u8,
    live_fetching: bool,
    panels: Panels,
    help: bool,
    /// Highest realtime count seen this session — the meter's high-water mark.
    peak: f64,
    /// Drives every phase-based effect, so they all share one clock.
    started: Instant,
    last_report: Instant,
    report_every: Duration,
    live_every: Duration,
    /// Whether to wear the subscriber star in the header.
    supporter: bool,
}

impl Dash {
    fn new(
        title: String,
        days: u32,
        snapshot: Snapshot,
        live: f64,
        realms: Vec<(String, f64)>,
        report_every: Duration,
        live_every: Duration,
    ) -> Dash {
        let mut dash = Dash {
            title,
            days,
            metrics: Vec::new(),
            daily: Vec::new(),
            pages: Vec::new(),
            events: EventTrend::default(),
            live: Eased::new(live),
            live_raw: live,
            history: VecDeque::from(vec![live]),
            realms: Vec::new(),
            live_realms: realms,
            feed: VecDeque::new(),
            updated: stamp(),
            error: None,
            in_flight: 0,
            live_fetching: false,
            panels: Panels {
                vitals: true,
                live: true,
                chunks: true,
                realms_ranked: true,
                trend: true,
                map: true,
                events: true,
            },
            help: false,
            peak: live.max(1.0),
            started: Instant::now(),
            last_report: Instant::now(),
            report_every,
            live_every,
            supporter: false,
        };
        dash.apply_report(snapshot);
        // The constructor's own report shouldn't set every row flashing.
        for row in &mut dash.metrics {
            row.flash = None;
        }
        dash
    }

    /// Re-point the dashboard at another property. The numbers on screen
    /// belong to the property we are leaving, so anything that is a running
    /// tally of *this* site resets; the metric rows are left in place so the
    /// eased values animate across instead of blanking the layout.
    fn switch_to(&mut self, title: String, settings: Settings) {
        self.title = title;
        self.days = settings.days;
        self.report_every = Duration::from_secs(settings.refresh.max(5));
        self.live_every = Duration::from_secs(settings.live_refresh.max(LIVE_FLOOR));
        self.error = None;
        self.feed.clear();
        self.history.clear();
        self.history.push_back(self.live_raw);
        self.peak = self.live_raw.max(1.0);
        for row in &mut self.metrics {
            row.flash = None;
        }
    }

    fn apply_report(&mut self, snapshot: Snapshot) {
        self.apply_totals(snapshot.current, snapshot.previous);
        self.apply_trend(snapshot.daily);
        self.apply_pages(snapshot.pages);
        self.apply_events(snapshot.events);
        self.realms = snapshot.realms;
    }

    fn apply_totals(&mut self, current: Vec<f64>, previous: Vec<f64>) {
        let now = Instant::now();

        for (i, _) in OVERVIEW.iter().enumerate() {
            let current = current.get(i).copied().unwrap_or(0.0);
            let previous = previous.get(i).copied().unwrap_or(0.0);
            let frac = crate::render::bar_fraction(OVERVIEW[i], current, previous);

            match self.metrics.get_mut(i) {
                Some(row) => {
                    if row.value.target != current {
                        row.flash = Some(now);
                    }
                    row.value.to(current);
                    row.frac.to(frac);
                    row.previous = previous;
                }
                None => self.metrics.push(MetricRow {
                    value: Eased::new(current),
                    frac: Eased::new(frac),
                    previous,
                    flash: Some(now),
                }),
            }
        }

        self.updated = stamp();
        self.error = None;
    }

    fn apply_trend(&mut self, daily: Vec<f64>) {
        self.daily = daily;
        self.updated = stamp();
        self.error = None;
    }

    fn apply_pages(&mut self, pages: Vec<(String, f64)>) {
        let peak = pages.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);

        // The board as it stood, so a chunk that climbed can say so.
        let before: Vec<String> = self.pages.iter().map(|row| row.path.clone()).collect();
        let had_board = !before.is_empty();

        self.pages.truncate(pages.len());
        for (i, (path, views)) in pages.iter().enumerate() {
            let frac = if peak > 0.0 { views / peak } else { 0.0 };
            let moved = match before.iter().position(|seen| seen == path) {
                Some(was) => Some(was as i64 - i as i64),
                // Only call something new if there was a board to be new to.
                None if had_board => None,
                None => Some(0),
            };
            match self.pages.get_mut(i) {
                // Rows are positional: when a page changes rank, the label
                // swaps and the bar animates from whatever was there before,
                // which reads as the ranking rearranging itself.
                Some(row) => {
                    row.path = path.clone();
                    row.views.to(*views);
                    row.frac.to(frac);
                    row.moved = moved;
                }
                None => self.pages.push(PageRow {
                    path: path.clone(),
                    views: Eased::new(*views),
                    frac: Eased::new(frac),
                    moved,
                }),
            }
        }

        self.updated = stamp();
        self.error = None;
    }

    /// The two periods are plotted against one x range, so day 1 of this period
    /// sits under day 1 of the last however many days each actually returned.
    fn apply_events(&mut self, (current, previous): EventCounts) {
        let points = |counts: &[(String, f64)]| -> Vec<(f64, f64)> {
            counts
                .iter()
                .enumerate()
                .map(|(i, (_, count))| (i as f64, *count))
                .collect()
        };

        self.events = EventTrend {
            days: current.iter().map(|(date, _)| day_of_month(date)).collect(),
            total: current.iter().map(|(_, count)| count).sum(),
            total_previous: previous.iter().map(|(_, count)| count).sum(),
            // One scale for both lines, or the comparison is meaningless.
            peak: current
                .iter()
                .chain(previous.iter())
                .map(|(_, count)| *count)
                .fold(0.0_f64, f64::max),
            current: points(&current),
            previous: points(&previous),
        };

        self.updated = stamp();
        self.error = None;
    }

    fn apply_live(&mut self, live: f64, realms: Vec<(String, f64)>) {
        self.live_realms = realms;
        let delta = live - self.live_raw;
        if delta != 0.0 {
            self.feed.push_front(FeedEvent {
                delta,
                at: Instant::now(),
            });
        }
        self.live_raw = live;
        self.live.to(live);
        self.peak = self.peak.max(live);
    }

    /// Samples the *displayed* value onto the graph. Called on its own clock,
    /// not on the poll: that is what keeps the trace scrolling — and easing
    /// between polls — instead of standing still until the next sample lands.
    fn trace(&mut self) {
        self.history.push_back(self.live.shown);
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }

    fn step(&mut self, dt: f64) {
        for row in &mut self.metrics {
            row.value.step(dt);
            row.frac.step(dt);
        }
        for row in &mut self.pages {
            row.views.step(dt);
            row.frac.step(dt);
        }
        self.live.step(dt);
        while self.feed.back().is_some_and(|e| e.at.elapsed() > FEED_TTL) {
            self.feed.pop_back();
        }
    }

    /// Seconds-based clock every phase effect reads from.
    fn phase(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

fn stamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

// ------------------------------------------------------------------ fetch ---

/// The headline totals and the period they are measured against.
async fn fetch_totals(client: &Ga, property: &str, days: u32) -> Result<(Vec<f64>, Vec<f64>)> {
    let metrics: Vec<&str> = OVERVIEW.iter().map(|m| m.api).collect();

    let (current, previous) = tokio::try_join!(
        client.report(
            property,
            ReportRequest::new(&metrics).range(DateRange::last_days(days)),
        ),
        client.report(
            property,
            ReportRequest::new(&metrics).range(DateRange::previous_days(days)),
        )
    )?;

    Ok((
        (0..OVERVIEW.len()).map(|i| current.total(i)).collect(),
        (0..OVERVIEW.len()).map(|i| previous.total(i)).collect(),
    ))
}

async fn fetch_trend(client: &Ga, property: &str, days: u32) -> Result<Vec<f64>> {
    let trend = client
        .report(
            property,
            ReportRequest::new(&["totalUsers"])
                .by(&["date"])
                .range(DateRange::last_days(days)),
        )
        .await?;

    // GA returns date rows unordered; the sparkline needs them chronological.
    let mut rows = trend.rows.clone();
    rows.sort_by(|a, b| a.dimension(0).cmp(b.dimension(0)));
    Ok(rows.iter().map(|r| r.metric(0)).collect())
}

/// Users by country over the period — the map's base layer.
async fn fetch_realms(client: &Ga, property: &str, days: u32) -> Result<Vec<(String, f64)>> {
    let report = client
        .report(
            property,
            ReportRequest::new(&["totalUsers"])
                .by(&["country"])
                .range(DateRange::last_days(days))
                .top("totalUsers", 40),
        )
        .await?;

    Ok(report
        .rows
        .iter()
        .map(|r| (r.dimension(0).to_string(), r.metric(0)))
        .collect())
}

async fn fetch_pages(client: &Ga, property: &str, days: u32) -> Result<Vec<(String, f64)>> {
    let pages = client
        .report(
            property,
            ReportRequest::new(&["screenPageViews"])
                .by(&["pagePath"])
                .range(DateRange::last_days(days))
                .top("screenPageViews", 8),
        )
        .await?;

    Ok(pages
        .rows
        .iter()
        .map(|r| (r.dimension(0).to_string(), r.metric(0)))
        .collect())
}

/// Event counts per day, for the period and for the period before it — the pair
/// the chart draws as one comparison.
///
/// Two requests rather than one: GA has no period comparison in a single report,
/// and asking for both date ranges at once returns them interleaved with no way
/// to tell which range a row came from.
async fn fetch_events(client: &Ga, property: &str, days: u32) -> Result<EventCounts> {
    let by_day = |range| {
        ReportRequest::new(&["eventCount"])
            .by(&["date"])
            .range(range)
    };

    let (current, previous) = tokio::try_join!(
        client.report(property, by_day(DateRange::last_days(days))),
        client.report(property, by_day(DateRange::previous_days(days)))
    )?;

    // GA returns date rows unordered; a line chart needs them chronological.
    let series = |report: &crate::ga::Report| {
        let mut rows: Vec<(String, f64)> = report
            .rows
            .iter()
            .map(|row| (row.dimension(0).to_string(), row.metric(0)))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    };

    Ok((series(&current), series(&previous)))
}

/// `YYYYMMDD` down to the day, for the chart's x axis.
fn day_of_month(date: &str) -> String {
    date.get(6..8)
        .map(|day| day.trim_start_matches('0').to_string())
        .filter(|day| !day.is_empty())
        .unwrap_or_else(|| date.to_string())
}

/// The whole set at once, for the fetch that happens before the screen is
/// taken over. The parts run concurrently, so this costs one round trip rather
/// than one per part.
async fn fetch_report(client: &Ga, property: &str, days: u32) -> Result<Snapshot> {
    let (totals, daily, pages, realms, events) = tokio::try_join!(
        fetch_totals(client, property, days),
        fetch_trend(client, property, days),
        fetch_pages(client, property, days),
        fetch_realms(client, property, days),
        fetch_events(client, property, days)
    )?;

    Ok(Snapshot {
        current: totals.0,
        previous: totals.1,
        daily,
        pages,
        realms,
        events,
    })
}

/// The realtime count and its breakdown by country, which is what the map
/// lights up from. Rows are summed rather than read off `totals`: a dimensioned
/// realtime request doesn't come back with aggregates.
async fn fetch_live(client: &Ga, property: &str) -> Result<(f64, Vec<(String, f64)>)> {
    let report = client
        .realtime(
            property,
            ReportRequest::new(&["activeUsers"])
                .by(&["country"])
                .top("activeUsers", 30),
        )
        .await?;

    let realms: Vec<(String, f64)> = report
        .rows
        .iter()
        .map(|r| (r.dimension(0).to_string(), r.metric(0)))
        .collect();
    let total = realms.iter().map(|(_, users)| users).sum();
    Ok((total, realms))
}

/// Where the dashboard's numbers come from. The event loop is written against
/// this rather than against `Ga`, so `dash --demo` exercises exactly the same
/// animation path as a connected property.
enum Source {
    Api { client: Arc<Ga>, property: String },
    Demo(std::sync::Mutex<Synthetic>),
}

/// The requests a report pass is made of.
#[derive(Clone, Copy)]
enum Part {
    Totals,
    Trend,
    Pages,
    Realms,
    Events,
}

impl Source {
    /// Kicks off a report pass and returns how many parts to expect back. The
    /// parts run concurrently and each is sent the moment it lands, so the
    /// dashboard fills in as data comes in instead of in one jump at the end.
    fn request_report(&self, days: u32, tx: &UnboundedSender<Update>) -> u8 {
        match self {
            Source::Api { client, property } => {
                let spawn = |part: Part| {
                    let (client, property, tx) = (client.clone(), property.clone(), tx.clone());
                    tokio::spawn(async move {
                        let update = match part {
                            Part::Totals => fetch_totals(&client, &property, days)
                                .await
                                .map(|(current, previous)| Update::Totals { current, previous }),
                            Part::Trend => fetch_trend(&client, &property, days)
                                .await
                                .map(Update::Trend),
                            Part::Pages => fetch_pages(&client, &property, days)
                                .await
                                .map(Update::Pages),
                            Part::Realms => fetch_realms(&client, &property, days)
                                .await
                                .map(Update::Realms),
                            Part::Events => fetch_events(&client, &property, days)
                                .await
                                .map(Update::Events),
                        };
                        // Keep showing stale numbers rather than tearing the
                        // screen down.
                        let _ =
                            tx.send(update.unwrap_or_else(|err| Update::Failed(err.to_string())));
                    });
                };
                spawn(Part::Totals);
                spawn(Part::Trend);
                spawn(Part::Pages);
                spawn(Part::Realms);
                spawn(Part::Events);
                5
            }
            Source::Demo(synthetic) => {
                let snapshot = synthetic.lock().unwrap().report(&mut rand::thread_rng());
                let _ = tx.send(Update::Totals {
                    current: snapshot.current,
                    previous: snapshot.previous,
                });
                let _ = tx.send(Update::Trend(snapshot.daily));
                let _ = tx.send(Update::Pages(snapshot.pages));
                let _ = tx.send(Update::Realms(snapshot.realms));
                let _ = tx.send(Update::Events(snapshot.events));
                5
            }
        }
    }

    /// Point an API source at a different property. Inert for the demo, which
    /// has only its synthetic site.
    fn set_property(&mut self, id: &str) {
        if let Source::Api { property, .. } = self {
            *property = id.to_string();
        }
    }

    fn request_live(&self, tx: &UnboundedSender<Update>) {
        match self {
            Source::Api { client, property } => {
                let (client, property, tx) = (client.clone(), property.clone(), tx.clone());
                tokio::spawn(async move {
                    // Realtime is the flakiest endpoint of the set; a failure
                    // there holds the last count rather than papering the
                    // dashboard with an error.
                    if let Ok((total, realms)) = fetch_live(&client, &property).await {
                        let _ = tx.send(Update::Live { total, realms });
                    }
                });
            }
            Source::Demo(synthetic) => {
                let (total, realms) = synthetic.lock().unwrap().live(&mut rand::thread_rng());
                let _ = tx.send(Update::Live { total, realms });
            }
        }
    }
}

// ------------------------------------------------------------------- loop ---

/// Cadence and window the dashboard runs at, after flags and the property's
/// own saved settings have been folded together.
#[derive(Clone, Copy)]
pub struct Settings {
    pub days: u32,
    pub refresh: u64,
    pub live_refresh: u64,
}

impl Settings {
    /// This property's overrides on top of the resolved defaults. Switching to
    /// a property that saved nothing lands back on the defaults rather than
    /// inheriting whatever the previous property used.
    fn for_property(&self, property: &Property) -> Settings {
        Settings {
            days: property.days.unwrap_or(self.days),
            refresh: property.refresh.unwrap_or(self.refresh),
            live_refresh: property.live_refresh.unwrap_or(self.live_refresh),
        }
    }
}

pub async fn run(cfg: &Config, property: &str, settings: Settings) -> Result<()> {
    let client = Arc::new(Ga::new()?);

    // Tab cycles this list. Start it on the property we were asked for, so
    // `--property` decides where the dashboard opens, not just what it can
    // reach. A property that isn't in the config still runs, alone.
    let mut rotation: Vec<Property> = cfg.properties.clone();
    if !rotation.iter().any(|p| p.id == property) {
        rotation.insert(
            0,
            Property {
                id: property.to_string(),
                ..Property::default()
            },
        );
    }
    let index = rotation
        .iter()
        .position(|p| p.id == property)
        .unwrap_or_default();
    let opening = settings.for_property(&rotation[index]);
    let title = rotation[index].display();

    // Fetch before taking over the screen so auth/API errors print normally.
    let snapshot = fetch_report(&client, property, opening.days).await?;
    let (live, realms) = fetch_live(&client, property)
        .await
        .unwrap_or((0.0, Vec::new()));

    let source = Source::Api {
        client,
        property: property.to_string(),
    };
    drive(
        source,
        title,
        snapshot,
        live,
        realms,
        settings,
        rotation,
        index,
        cfg.supporter,
    )
    .await
}

/// The dashboard on synthetic data — no account, but the same code path, which
/// is what makes it usable for screenshots and for tuning the animation.
pub async fn run_demo(days: u32, refresh: u64, live_refresh: u64) -> Result<()> {
    let mut synthetic = Synthetic::new();
    let mut rng = rand::thread_rng();
    let snapshot = synthetic.report(&mut rng);
    let (live, realms) = synthetic.live(&mut rng);

    let source = Source::Demo(std::sync::Mutex::new(synthetic));
    drive(
        source,
        "Contoso Labs (demo)".to_string(),
        snapshot,
        live,
        realms,
        Settings {
            days,
            refresh,
            live_refresh,
        },
        Vec::new(),
        0,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    source: Source,
    title: String,
    snapshot: Snapshot,
    live: f64,
    realms: Vec<(String, f64)>,
    settings: Settings,
    rotation: Vec<Property>,
    index: usize,
    supporter: bool,
) -> Result<()> {
    let opening = match rotation.get(index) {
        Some(property) => settings.for_property(property),
        None => settings,
    };
    let mut source = source;
    let mut dash = Dash::new(
        title,
        opening.days,
        snapshot,
        live,
        realms,
        Duration::from_secs(opening.refresh.max(5)),
        Duration::from_secs(opening.live_refresh.max(LIVE_FLOOR)),
    );
    dash.supporter = supporter;

    let mut terminal = ratatui::init();
    // Ctrl+digit only reaches an application in terminals that speak the Kitty
    // keyboard protocol; without this they send the bare digit (or nothing).
    // The plain digits keep working either way, so this is an upgrade rather
    // than a requirement.
    let enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    if enhanced {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let result = event_loop(
        &mut terminal,
        &mut source,
        &mut dash,
        &rotation,
        index,
        settings,
    )
    .await;
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    // Persist the theme the user settled on — so the next launch starts there.
    // It belongs to the property they were looking at when they pressed `t`,
    // not to every property at once.
    if let Ok(mut cfg) = crate::config::Config::load() {
        let name = theme::palette().name.to_string();
        match cfg.active.clone().filter(|id| cfg.find(id).is_some()) {
            Some(id) => cfg.upsert(&id, None).theme = Some(name),
            None => cfg.theme = Some(name),
        }
        let _ = cfg.save();
    }
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    source: &mut Source,
    dash: &mut Dash,
    rotation: &[Property],
    mut index: usize,
    settings: Settings,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut last_live = Instant::now();
    let mut last_frame = Instant::now();
    let mut last_trace = Instant::now();

    loop {
        // Everything waiting is applied before the frame is drawn, so a part
        // that lands mid-frame shows up on the very next one.
        while let Ok(update) = rx.try_recv() {
            match update {
                Update::Totals { current, previous } => {
                    dash.apply_totals(current, previous);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Trend(daily) => {
                    dash.apply_trend(daily);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Pages(pages) => {
                    dash.apply_pages(pages);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Events(events) => {
                    dash.apply_events(events);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Realms(realms) => {
                    dash.realms = realms;
                    dash.updated = stamp();
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Live { total, realms } => {
                    dash.apply_live(total, realms);
                    dash.live_fetching = false;
                }
                Update::Failed(err) => {
                    dash.error = Some(err);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
            }
        }

        let now = Instant::now();
        let dt = now.duration_since(last_frame).as_secs_f64();
        last_frame = now;
        dash.step(dt);
        if last_trace.elapsed() >= TRACE_EVERY {
            dash.trace();
            last_trace = now;
        }

        terminal.draw(|frame| draw(frame, dash))?;

        if dash.in_flight == 0 && dash.last_report.elapsed() >= dash.report_every {
            dash.in_flight = source.request_report(dash.days, &tx);
            dash.last_report = now;
        }
        if !dash.live_fetching && last_live.elapsed() >= dash.live_every {
            source.request_live(&tx);
            dash.live_fetching = true;
            last_live = now;
        }

        // The poll timeout is the frame budget: input wakes us early, and
        // otherwise this is the tick that advances the animation.
        if event::poll(FRAME)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(())
                        }
                        KeyCode::Char('q') => return Ok(()),
                        // Esc closes the help overlay first, so it isn't a
                        // surprise exit for anyone who opened it to look.
                        KeyCode::Esc if dash.help => dash.help = false,
                        KeyCode::Esc => return Ok(()),
                        KeyCode::Char('r') => {
                            dash.last_report = now - dash.report_every;
                            last_live = now - dash.live_every;
                        }
                        KeyCode::Char('?') | KeyCode::Char('h') => dash.help = !dash.help,
                        // Ctrl+digit and the bare digit do the same thing: the
                        // titles advertise Ctrl, but not every terminal can
                        // send it.
                        KeyCode::Char('1') | KeyCode::Char('e') => {
                            dash.panels.events = !dash.panels.events
                        }
                        KeyCode::Char('2') | KeyCode::Char('l') => {
                            dash.panels.live = !dash.panels.live
                        }
                        KeyCode::Char('3') | KeyCode::Char('m') => {
                            dash.panels.map = !dash.panels.map
                        }
                        KeyCode::Char('4') | KeyCode::Char('p') => {
                            dash.panels.chunks = !dash.panels.chunks
                        }
                        KeyCode::Char('5') | KeyCode::Char('v') => {
                            dash.panels.vitals = !dash.panels.vitals
                        }
                        KeyCode::Char('6') | KeyCode::Char('g') => {
                            dash.panels.realms_ranked = !dash.panels.realms_ranked
                        }
                        KeyCode::Char('7') | KeyCode::Char('d') => {
                            dash.panels.trend = !dash.panels.trend
                        }
                        // Nothing to announce: every color on screen changes,
                        // which is the feedback.
                        KeyCode::Char('t') => {
                            theme::cycle();
                        }
                        // Tab walks the configured properties. A one-property
                        // rotation has nothing to walk to, so the key is inert
                        // rather than redrawing the same numbers.
                        KeyCode::Tab | KeyCode::BackTab if rotation.len() > 1 => {
                            let step = if key.code == KeyCode::Tab {
                                1
                            } else {
                                rotation.len() - 1
                            };
                            index = (index + step) % rotation.len();
                            let next = &rotation[index];
                            let resolved = settings.for_property(next);

                            source.set_property(&next.id);
                            dash.switch_to(next.display(), resolved);
                            // A property carrying its own palette should show it
                            // immediately, not on the next launch.
                            if let Some(name) = next.theme.as_deref() {
                                theme::select(name);
                            }

                            // Re-fetch at once: the numbers on screen belong to
                            // the property we just left.
                            dash.in_flight = source.request_report(dash.days, &tx);
                            dash.last_report = Instant::now();
                            source.request_live(&tx);
                            dash.live_fetching = true;
                            last_live = Instant::now();
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------- synthetic ---

/// A small site having a good week, drifting on every poll so the demo shows
/// motion rather than a frozen frame.
struct Synthetic {
    current: Vec<f64>,
    previous: Vec<f64>,
    daily: Vec<f64>,
    live: f64,
}

impl Synthetic {
    fn new() -> Synthetic {
        Synthetic {
            current: vec![12_481.0, 18_203.0, 41_776.0, 312.0, 0.412, 214.0],
            previous: vec![11_450.0, 17_004.0, 39_210.0, 258.0, 0.478, 191.0],
            daily: vec![1402.0, 1288.0, 1531.0, 1495.0, 1760.0, 1834.0, 1971.0],
            live: 128.0,
        }
    }

    fn report(&mut self, rng: &mut impl Rng) -> Snapshot {
        // Counts creep up, the bounce rate wobbles, the day's last bar grows.
        for (i, value) in self.current.iter_mut().enumerate() {
            *value *= 1.0 + rng.gen_range(-0.004..0.012) * if i == 4 { 0.4 } else { 1.0 };
        }
        if let Some(today) = self.daily.last_mut() {
            *today *= 1.0 + rng.gen_range(-0.02..0.05);
        }

        let mut snapshot = Snapshot {
            current: self.current.clone(),
            previous: self.previous.clone(),
            daily: self.daily.clone(),
            pages: [
                "/",
                "/pricing",
                "/docs/quickstart",
                "/blog/mining-metrics",
                "/changelog",
                "/docs/api",
                "/about",
                "/login",
            ]
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let base = 9_400.0 / (i as f64 + 1.4);
                // Wide enough that neighbouring chunks trade places now and
                // then, which is the only way to see the movement markers.
                (path.to_string(), base * rng.gen_range(0.80..1.20))
            })
            .collect(),
            realms: [
                ("United States", 0.34),
                ("India", 0.14),
                ("Germany", 0.09),
                ("United Kingdom", 0.08),
                ("Brazil", 0.07),
                ("Japan", 0.06),
                ("Canada", 0.05),
                ("Australia", 0.04),
                ("Nigeria", 0.04),
                ("France", 0.03),
                ("Sweden", 0.02),
                ("Singapore", 0.02),
                ("South Africa", 0.02),
                ("Mexico", 0.02),
            ]
            .iter()
            .map(|(name, share)| {
                (
                    name.to_string(),
                    (self.current[0] * share * rng.gen_range(0.9..1.1)).round(),
                )
            })
            .collect(),
            events: {
                // A fixed anchor date, not today's: the site's captures embed
                // these day labels, and a moving window would rewrite them on
                // every regeneration.
                let anchor = chrono::NaiveDate::from_ymd_opt(2026, 8, 19).unwrap();
                // A week that sags at the weekend and climbs into Monday. Shaped
                // rather than random, so the two periods cross somewhere and the
                // comparison has something to show.
                const NOW: [f64; 7] = [0.61, 0.72, 0.54, 0.33, 0.44, 0.87, 1.00];
                const BEFORE: [f64; 7] = [0.57, 0.48, 0.60, 0.38, 0.30, 0.66, 0.71];
                // Events outnumber page views: every view is one, plus the rest.
                let base = self.current[2] * 1.7 / 7.0;
                let series = |shape: &[f64; 7], offset: i64| -> Vec<(String, f64)> {
                    shape
                        .iter()
                        .enumerate()
                        .map(|(day, scale)| {
                            let back = offset + 6 - day as i64;
                            let date = anchor - chrono::Duration::days(back);
                            (date.format("%Y%m%d").to_string(), (base * scale).round())
                        })
                        .collect()
                };
                (series(&NOW, 0), series(&BEFORE, 7))
            },
        };

        // GA returns these ranked; the jitter above would otherwise leave them
        // in their original order with the values out of sequence.
        snapshot.pages.sort_by(|a, b| b.1.total_cmp(&a.1));
        snapshot
    }

    fn live(&mut self, rng: &mut impl Rng) -> (f64, Vec<(String, f64)>) {
        // Random walk with a pull back toward 128, so it wanders without
        // drifting off the panel.
        let pull = (128.0 - self.live) * 0.15;
        self.live = (self.live + pull + rng.gen_range(-9.0..9.0))
            .max(0.0)
            .round();

        // Split the walkers over a plausible spread of countries, so the map
        // has something to light up.
        let realms = [
            ("United States", 0.34),
            ("India", 0.14),
            ("Germany", 0.09),
            ("United Kingdom", 0.08),
            ("Brazil", 0.07),
            ("Japan", 0.06),
            ("Canada", 0.05),
            ("Australia", 0.04),
            ("Nigeria", 0.04),
            ("France", 0.03),
        ]
        .iter()
        .map(|(name, share)| {
            let jitter = rng.gen_range(0.85..1.15);
            (name.to_string(), (self.live * share * jitter).round())
        })
        .collect();
        (self.live, realms)
    }
}

// ---------------------------------------------------------------- capture ---

/// The captures the site embeds: the wide one and the phone-sized reflow.
const CAPTURES: [(u16, u16); 2] = [(132, 52), (74, 58)];
/// Fixed, so `make capture` produces the same numbers every run and a regenerated
/// site is a diff of what actually changed rather than of fresh demo jitter.
const CAPTURE_SEED: u64 = 0x0a0e_c4af;

/// The demo dashboard as the site shows it: eased fully into place, so nothing
/// is captured half-animated.
fn capture_dash() -> Dash {
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(CAPTURE_SEED);

    let mut synthetic = Synthetic::new();
    let snapshot = synthetic.report(&mut rng);
    let (live, realms) = synthetic.live(&mut rng);
    let mut dash = Dash::new(
        "Contoso Labs (demo)".to_string(),
        7,
        snapshot,
        live,
        realms,
        Duration::from_secs(30),
        Duration::from_secs(5),
    );

    // Run the realtime poll forward for a while before capturing. A cold start
    // has an empty trace and an empty feed, so capturing one shows the realtime
    // panel with a flat line and "quiet out there" — the two things it exists to
    // disprove.
    for tick in 0..HISTORY {
        if tick % 8 == 0 {
            let (live, realms) = synthetic.live(&mut rng);
            dash.apply_live(live, realms);
        }
        dash.step(FRAME.as_secs_f64());
        dash.trace();
    }
    // Settle whatever is still easing, so nothing is caught mid-flight.
    for _ in 0..120 {
        dash.step(FRAME.as_secs_f64());
    }
    // The footer stamps the wall clock, which would otherwise be the one thing
    // that differs every time the captures are regenerated.
    dash.updated = "17:24:53".to_string();
    dash
}

/// One layer of a capture. The site paints cell backgrounds and glyphs as two
/// stacked plates, the order a terminal composites in — see the note on `.dash`
/// in `docs/index.html` for why a single plate cannot hold both.
fn plate(buffer: &Buffer, background: bool) -> String {
    let area = *buffer.area();
    let mut out = String::new();

    for y in 0..area.height {
        if y > 0 {
            out.push('\n');
        }
        // Runs of cells sharing a color collapse into one tag, or the page
        // would carry a span per cell and weigh several megabytes.
        let mut run = String::new();
        let mut key: Option<(Option<String>, bool)> = None;

        let flush = |out: &mut String, run: &mut String, key: &Option<(Option<String>, bool)>| {
            if run.is_empty() {
                return;
            }
            let text = escape(run, !background);
            match key {
                Some((Some(hex), bold)) => {
                    let property = if background { "background" } else { "color" };
                    let weight = if *bold { ";font-weight:700" } else { "" };
                    out.push_str(&format!("<b style=\"{property}:{hex}{weight}\">{text}</b>"));
                }
                // A cell the theme never colored: the plate leaves it bare.
                _ => out.push_str(&text),
            }
            run.clear();
        };

        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            let color = if background { cell.bg } else { cell.fg };
            let bold = !background && cell.modifier.contains(Modifier::BOLD);
            let next = (hex(color), bold);
            if key.as_ref() != Some(&next) {
                flush(&mut out, &mut run, &key);
                key = Some(next);
            }
            run.push_str(cell.symbol());
        }
        flush(&mut out, &mut run, &key);
    }

    out
}

/// `Color::Rgb` is all the palettes use, so anything else is a cell nobody
/// styled and the plate leaves it to the page's own background.
fn hex(color: Color) -> Option<String> {
    match color {
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        _ => None,
    }
}

/// HTML-escapes a run, and pins the pickaxe's width on the glyph plate: U+26CF
/// is absent from JetBrains Mono and falls back to an emoji wider than its cell,
/// which would shift everything after it out of the grid.
fn escape(text: &str, pin_wide: bool) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if pin_wide {
        escaped.replace(
            glyph::PICKAXE,
            &format!(
                "<i class=\"wide\" style=\"width:0.6em\">{}</i>",
                glyph::PICKAXE
            ),
        )
    } else {
        escaped
    }
}

/// Renders the demo dashboard for every palette, at both captures, as the
/// two-plate HTML `docs/index.html` embeds between its capture markers.
///
/// The site used to carry these by hand, which is how it came to advertise a
/// panel the dashboard had stopped drawing and a palette it never shipped.
pub fn capture() -> Result<String> {
    use ratatui::backend::TestBackend;

    let mut out = String::new();

    for (index, (width, height)) in CAPTURES.iter().enumerate() {
        let class = if index == 0 { "wide" } else { "narrow" };
        out.push_str(&format!("<div class=\"plate {class}\">"));

        for (nth, palette) in theme::THEMES.iter().enumerate() {
            if !theme::select(palette.name) {
                anyhow::bail!("no palette named {}", palette.name);
            }
            let dash = capture_dash();

            let mut terminal = Terminal::new(TestBackend::new(*width, *height))?;
            terminal.draw(|frame| draw(frame, &dash))?;
            let buffer = terminal.backend().buffer();

            // Only the first is shown; the tabs unhide the others.
            let hidden = if nth == 0 { "" } else { " hidden" };
            out.push_str(&format!(
                "<pre class=\"dash\" data-theme=\"{}\"{hidden}>\
                 <span class=\"lyr bgl\" aria-hidden=\"true\">{}</span>\
                 <span class=\"lyr fgl\">{}</span></pre>",
                palette.name,
                plate(buffer, true),
                plate(buffer, false),
            ));
        }

        out.push_str("</div>\n");
    }

    Ok(out)
}

// ------------------------------------------------------------------- draw ---

/// What the dashboard shows instead of itself when the terminal is too small.
///
/// Names both numbers and marks only the one that is short, so the fix is
/// obvious without having to compare four figures. Nothing animates: this is a
/// screen you are meant to read once and leave, and a pulsing dot on it would
/// suggest the dashboard is still working on something.
fn too_small(area: Rect) -> Paragraph<'static> {
    let short = |have: u16, need: u16| {
        if have < need {
            Style::default()
                .fg(ore::redstone())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::fade(theme::sage(), 0.3))
        }
    };

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {} ANACRAFT ", glyph::PICKAXE),
                Style::default()
                    .fg(ore::grass())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "· the shaft is too tight to work in",
                Style::default().fg(ore::stone()),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  have  ", Style::default().fg(ore::stone())),
            Span::styled(format!("{:>4}", area.width), short(area.width, MIN_COLS)),
            Span::styled(" × ", Style::default().fg(ore::netherite())),
            Span::styled(format!("{:<4}", area.height), short(area.height, MIN_ROWS)),
        ]),
        Line::from(vec![
            Span::styled("  needs ", Style::default().fg(ore::stone())),
            Span::styled(
                format!("{MIN_COLS:>4}"),
                Style::default().fg(theme::accent()),
            ),
            Span::styled(" × ", Style::default().fg(ore::netherite())),
            Span::styled(
                format!("{MIN_ROWS:<4}"),
                Style::default().fg(theme::accent()),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  resize the terminal · q to quit",
            Style::default().fg(theme::fade(theme::sage(), 0.3)),
        )),
    ];

    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ore::netherite()))
            .style(Style::default().bg(theme::bg_lift())),
    )
}

/// Whether this install is an Anacrafter, and how to become one if not.
///
/// A box of its own rather than a slot in the footer hotbar. The footer is
/// where the keybinds live and it runs out of room on a narrow terminal, so an
/// ask parked at the end of it is the first thing to disappear on exactly the
/// setups least likely to have seen it before. This is the one line in the
/// dashboard that pays for the rest, so it gets space that nothing else can
/// take.
///
/// Both states use the same box, because the answer to "am I an Anacrafter" is
/// worth stating either way — one line is a thank-you, the other is an ask.
fn supporter_box(dash: &Dash) -> Paragraph<'static> {
    let star = Span::styled(
        format!("  {} ", glyph::STAR),
        Style::default()
            .fg(ore::gold())
            .add_modifier(Modifier::BOLD),
    );

    let line = if dash.supporter {
        Line::from(vec![
            star,
            Span::styled(
                "ANACRAFTER",
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  ·  thanks for keeping the lights on",
                Style::default().fg(ore::stone()),
            ),
        ])
    } else {
        Line::from(vec![
            star,
            Span::styled(
                "not an Anacrafter yet",
                Style::default().fg(theme::accent()),
            ),
            Span::styled("  ·  run ", Style::default().fg(ore::stone())),
            Span::styled(
                "craft subscribe",
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ·  $5/month", Style::default().fg(ore::stone())),
        ])
    };

    Paragraph::new(line).block(
        Block::default()
            .borders(Borders::ALL)
            // Gold when there is something to ask for, quiet once there isn't:
            // a permanent box should stop drawing the eye after it has been
            // answered.
            .border_style(Style::default().fg(if dash.supporter {
                ore::netherite()
            } else {
                theme::fade(ore::gold(), 0.45)
            }))
            .style(Style::default().bg(theme::bg_lift())),
    )
}

fn draw(frame: &mut Frame, dash: &Dash) {
    let area = frame.area();

    // Paint the Osaka Jade ground first: the darkest shade in the palette, so
    // the dashboard looks the same against any terminal background and the
    // panels above it have something to lift off.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::ink()).fg(theme::fg())),
        area,
    );

    if area.width < MIN_COLS || area.height < MIN_ROWS {
        frame.render_widget(too_small(area), area);
        return;
    }

    // Every layout below leaves a one-cell gutter. That gap is the ink ground
    // showing through, which is what makes the panels read as floating on it
    // rather than as one tiled surface.
    // An 80-column terminal has nothing to spare: the vitals rows clip their
    // delta first, so at that width the dashboard drops its outer margin and
    // widens the left panel rather than losing the numbers.
    let narrow = area.width < 100;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(SUPPORTER_ROWS),
            Constraint::Length(3),
        ])
        .margin(if narrow { 0 } else { 1 })
        .spacing(1)
        .split(area);

    frame.render_widget(header(dash, chunks[0].width), chunks[0]);
    body(frame, dash, chunks[1], narrow);
    frame.render_widget(supporter_box(dash), chunks[2]);
    frame.render_widget(footer(dash), chunks[3]);

    if dash.help {
        help_overlay(frame, area);
    }
}

/// Lays out whichever panels are switched on. A hidden panel doesn't leave a
/// hole — the remaining ones take its space, so `2` on a wide terminal turns
/// the dashboard into vitals beside a full-height chunk list.
fn body(frame: &mut Frame, dash: &Dash, area: Rect, narrow: bool) {
    if !dash.panels.any() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  every panel is hidden — 1, 2 or 3 brings one back, ? lists the keys",
                Style::default().fg(ore::stone()),
            )))
            .block(framed("ANACRAFT", "", ore::stone())),
            area,
        );
        return;
    }

    let right_any = dash.panels.right_any();
    let (left, right) = if dash.panels.vitals && right_any {
        let split = if narrow { (63, 37) } else { (56, 44) };
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(split.0),
                Constraint::Percentage(split.1),
            ])
            .spacing(1)
            .split(area);
        (Some(cols[0]), Some(cols[1]))
    } else if dash.panels.vitals {
        (Some(area), None)
    } else {
        (None, Some(area))
    };

    if let Some(rect) = left {
        // Rows are handed out in priority order, and a panel that cannot get
        // its full box is left out rather than squeezed into a few rows — the
        // same rule the right column follows.
        //
        // Events leads and is never the panel that gives way. It is the
        // headline chart the dashboard is named for, and a column that keeps a
        // table of vitals by compressing that chart into four rows has its
        // priorities backwards. Vitals goes last, so it is what a short
        // terminal loses — also the cheapest thing to lose, since every number
        // in it is one `craft overview` away.
        let mut budget = rect.height;
        let mut stack: Vec<(Stack, u16)> = Vec::new();

        // A box costs its own rows plus one for the gutter above it — and the
        // first box in the column has nothing above it, so it costs only its
        // rows. Charging the gutter unconditionally is how events came to be
        // rejected at exactly EVENTS_ROWS while the shorter map box slipped in
        // underneath it, which inverts the very priority this order sets.
        let mut cost = |needs: u16, placed: bool| -> bool {
            let needs = needs + u16::from(placed);
            let fits = budget >= needs;
            if fits {
                budget -= needs;
            }
            fits
        };

        // Rows are claimed in priority order — events, then vitals, then the
        // map. The map is the one that goes: it is the decorative panel of the
        // three, where vitals is the actual numbers, so a column with room for
        // two spends it on the chart and the figures and drops the picture.
        if dash.panels.events && cost(EVENTS_ROWS, false) {
            stack.push((Stack::Events, EVENTS_ROWS));
        }
        if cost(VITALS_ROWS, !stack.is_empty()) {
            stack.push((Stack::Vitals, VITALS_ROWS));
        }
        if dash.panels.map && cost(MAP_ROWS, !stack.is_empty()) {
            stack.push((Stack::Map, MAP_ROWS));
        }

        // Who gets rows is one question; where they sit is another. Sort back
        // into column order so changing the priority above never reshuffles the
        // dashboard — the map stays between the chart and the figures whenever
        // all three are up.
        stack.sort_by_key(|(panel, _)| match panel {
            Stack::Events => 0,
            Stack::Map => 1,
            Stack::Vitals => 2,
        });

        // Whatever ended up at the bottom takes the rows nobody claimed, so the
        // column never trails off into dead ground while the one beside it is
        // full. Everything above it is pinned to the height it asked for —
        // sharing the slack out would stretch every box a little instead.
        let last = stack.len().saturating_sub(1);
        let constraints: Vec<Constraint> = stack
            .iter()
            .enumerate()
            .map(|(i, (_, needs))| {
                if i == last {
                    Constraint::Min(*needs)
                } else {
                    Constraint::Length(*needs)
                }
            })
            .collect();

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .spacing(1)
            .split(rect);

        for ((panel, _), area) in stack.into_iter().zip(rows.iter()) {
            match panel {
                Stack::Map => frame.render_widget(map_panel(dash, area.width, area.height), *area),
                Stack::Events => frame.render_widget(events_panel(dash), *area),
                Stack::Vitals => frame.render_widget(metrics_panel(dash), *area),
            }
        }
    }

    let Some(rect) = right else { return };

    // The right column stacks whichever of its three panels are on, daily chart
    // at the bottom. Rows are handed out in priority order and a panel that
    // cannot get its minimum is left out entirely — a two-row box with its
    // contents squeezed away is worse than no box.
    let mut budget = rect.height;
    let mut panels = Vec::new();
    for (on, column, needs) in [
        (dash.panels.live, Column::Live, LIVE_ROWS),
        (dash.panels.chunks, Column::Chunks, CHUNKS_ROWS),
        (
            dash.panels.realms_ranked,
            Column::RealmsRanked,
            REALMS_RANKED_ROWS,
        ),
        (dash.panels.trend, Column::Trend, TREND_ROWS),
    ] {
        let gap = u16::from(!panels.is_empty());
        if on && budget >= needs + gap {
            budget -= needs + gap;
            panels.push((column, needs));
        }
    }
    if panels.is_empty() {
        return;
    }

    // Leftover rows go to the chunk list or realms ranked, which are the panels
    // that can use them; failing that, to whatever is first.
    let stretch = panels
        .iter()
        .position(|(column, _)| matches!(column, Column::Chunks | Column::RealmsRanked))
        .unwrap_or(0);
    let constraints: Vec<Constraint> = panels
        .iter()
        .enumerate()
        .map(|(i, (_, needs))| {
            if i == stretch {
                Constraint::Min(*needs)
            } else {
                Constraint::Length(*needs)
            }
        })
        .collect();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .spacing(1)
        .split(rect);
    let panels: Vec<Column> = panels.into_iter().map(|(column, _)| column).collect();

    for (panel, area) in panels.into_iter().zip(rows.iter()) {
        let widget = match panel {
            Column::Live => live_panel(dash, area.width),
            Column::Chunks => pages_panel(dash, area.width),
            Column::RealmsRanked => realms_ranked_panel(dash, area.width),
            Column::Trend => trend_panel(dash, area.width),
        };
        frame.render_widget(widget, *area);
    }
}

/// The key list, centered over whatever is on screen.
fn help_overlay(frame: &mut Frame, area: Rect) {
    let keys = [
        ("q / Esc", "quit"),
        ("r", "refresh now"),
        ("^1 / 1", "events panel"),
        ("^2 / 2", "right now panel"),
        ("^3 / 3", "countries map"),
        ("^4 / 4", "top pages panel"),
        ("^5 / 5", "vitals panel"),
        ("^6 / 6", "top countries"),
        ("^7 / 7", "daily users"),
        ("t", "next theme"),
        ("tab", "next property"),
        ("? / h", "this list"),
    ];

    let width = 40.min(area.width.saturating_sub(4));
    let height = (keys.len() as u16 + 3).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let mut lines = vec![Line::from("")];
    for (key, what) in keys {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<9}"),
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(what.to_string(), Style::default().fg(theme::fg())),
        ]));
    }

    // `Clear` first, or the panel underneath bleeds through the overlay.
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(framed("KEYS", "", ore::gold())),
        rect,
    );
}

/// A panel, captioned with the key that shows and hides it, set in a block —
/// the shortcut sits on the thing it acts on rather than only in the help
/// overlay.
fn framed(title: &str, key: &str, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        // One step up from the dashboard ground, so panels read as raised.
        .style(Style::default().bg(theme::bg()))
        .border_style(Style::default().fg(ore::netherite()))
        .title(Line::from(vec![
            Span::styled(
                if key.is_empty() {
                    " ".to_string()
                } else {
                    format!(" \u{25a0}^{key} ")
                },
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{title} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]))
}

/// Width the spinner needs when a fetch is in flight: two spaces and a glyph.
const SPINNER_COLUMN: usize = 3;

/// A short label for a realm chip.
///
/// Truncating to three characters renders "United States" and "United Kingdom"
/// as the same "Uni", so two different realms read identically in the header.
/// Multi-word names collapse to their initials instead — US, UK, UAE — while
/// single-word names keep their first three letters. Lowercase joining words
/// ("and", "of") are skipped so "Bosnia and Herzegovina" is BH, not BAH.
fn realm_abbrev(country: &str) -> String {
    let initials: String = country
        .split_whitespace()
        .filter(|word| word.chars().next().is_some_and(char::is_uppercase))
        .filter_map(|word| word.chars().next())
        .collect();

    if initials.chars().count() > 1 {
        initials
    } else {
        country.chars().take(3).collect()
    }
}

/// The country chips for the header, ordered by headcount and capped at five.
///
/// A chip is emitted only if it fits whole. Letting the terminal clip instead
/// leaves a half-written country against the border — "Bra:8" arriving as "B"
/// reads as a rendering fault rather than a boundary.
fn realm_chips(realms: &[(String, f64)], budget: usize) -> Vec<(String, String)> {
    let mut sorted: Vec<&(String, f64)> = realms.iter().collect();
    sorted.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut chips: Vec<(String, String)> = Vec::new();
    let mut used = 0usize;
    for (country, count) in sorted.iter().take(5) {
        let sep = if chips.is_empty() { " · " } else { "  " };
        let chip = format!("{}:{}", realm_abbrev(country), *count as u64);
        // Count columns, not bytes — a realm name is not necessarily ASCII.
        let width = sep.chars().count() + chip.chars().count();
        if used + width > budget {
            break;
        }
        used += width;
        chips.push((sep.to_string(), chip));
    }
    chips
}

fn header(dash: &Dash, width: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    // A slow sine breathes the live dot; the glyph steps through three sizes so
    // it still reads as a pulse on a terminal without truecolor.
    let breath = (phase * 2.2).sin() * 0.5 + 0.5;
    let dot = glyph::PULSE[((breath * 2.99) as usize).min(2)];

    let mut spans = vec![
        Span::styled(
            format!(" {} ANACRAFT ", glyph::PICKAXE),
            Style::default()
                .fg(ore::grass())
                .add_modifier(Modifier::BOLD),
        ),
        // A subscriber's star, in gold and only when earned. It rides beside the
        // brand rather than out at the end of the line, where the realm chips
        // spend whatever room is left and would eventually push it off screen.
        Span::styled(
            if dash.supporter {
                format!("{} ", glyph::STAR)
            } else {
                String::new()
            },
            Style::default()
                .fg(ore::gold())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} ", dash.title.to_uppercase()),
            Style::default()
                .fg(ore::diamond())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· last {} days ", dash.days),
            Style::default().fg(ore::stone()),
        ),
        Span::styled(
            format!("· {dot} "),
            Style::default().fg(theme::mix(theme::accent_deep(), theme::bright(), breath)),
        ),
        Span::styled(
            format!("{} online now", commas(dash.live.shown.round())),
            Style::default().fg(ore::xp()).add_modifier(Modifier::BOLD),
        ),
    ];

    // Top realms with their counts — where the players are right now. They get
    // whatever room is left inside the border, minus the space the spinner will
    // want if a fetch is in flight.
    let spinning = dash.in_flight > 0 || dash.live_fetching;
    let spent: usize = spans.iter().map(|span| span.width()).sum();
    let budget = (width as usize)
        .saturating_sub(2) // the block's own borders
        .saturating_sub(spent)
        .saturating_sub(if spinning { SPINNER_COLUMN } else { 0 });

    for (i, (sep, chip)) in realm_chips(&dash.live_realms, budget)
        .into_iter()
        .enumerate()
    {
        spans.push(Span::styled(sep, Style::default().fg(ore::netherite())));
        spans.push(Span::styled(
            chip,
            Style::default()
                .fg(theme::ramp(i))
                .add_modifier(Modifier::BOLD),
        ));
    }

    if spinning {
        spans.push(Span::styled(
            format!("  {}", spinner(phase)),
            Style::default().fg(theme::accent_deep()),
        ));
    }

    Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ore::netherite()))
            .style(Style::default().bg(theme::bg_lift())),
    )
}

fn metrics_panel(dash: &Dash) -> Paragraph<'static> {
    let phase = dash.phase();
    let mut lines: Vec<Line> = Vec::new();

    for (i, metric) in OVERVIEW.iter().enumerate() {
        let Some(row) = dash.metrics.get(i) else {
            continue;
        };
        let flash = flash_level(row.flash);
        // A row that just changed brightens and decays back to its own color,
        // so a refresh is visible even when the number barely moves.
        let label = theme::brighten((metric.color)(), flash * 0.7);

        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<14}", metric.craft),
                Style::default().fg(label).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<14}", format!("({})", metric.plain)),
                Style::default().fg(ore::stone()),
            ),
            Span::styled(
                format!("{:>10}  ", value(metric, row.value.shown)),
                Style::default()
                    .fg(theme::mix(theme::fg(), theme::bright(), flash))
                    .add_modifier(Modifier::BOLD),
            ),
            delta_span(row.value.target, row.previous, metric.api == "bounceRate"),
        ]));

        lines.push(Line::from(bar_spans(
            row.frac.shown,
            30,
            metric.glyph,
            (metric.color)(),
            phase,
            row.frac.moving(),
            2,
        )));
        lines.push(Line::from(""));
    }

    Paragraph::new(lines).block(framed("VITALS", "5", ore::grass()))
}

/// Rank badges: the top three chunks are ore, the rest are plain stone. It is
/// a leaderboard, so it may as well look like one.
fn tier(rank: usize) -> (char, Color) {
    match rank {
        0 => ('\u{25c6}', ore::diamond()),
        1 => ('\u{25c8}', ore::gold()),
        2 => ('\u{25c7}', ore::iron()),
        _ => ('\u{00b7}', ore::stone()),
    }
}

/// How far a chunk climbed or fell since the last report.
fn moved_span(moved: Option<i64>) -> Span<'static> {
    match moved {
        None => Span::styled(
            format!("{:>width$}", "NEW", width = MOVED_COLUMN),
            Style::default()
                .fg(ore::gold())
                .add_modifier(Modifier::BOLD),
        ),
        Some(0) | Some(i64::MIN..=-100) => Span::raw(" ".repeat(MOVED_COLUMN)),
        Some(places) => Span::styled(
            format!(
                "{:>width$}",
                format!(
                    "{}{}",
                    if places > 0 { glyph::UP } else { glyph::DOWN },
                    places.abs()
                ),
                width = MOVED_COLUMN
            ),
            Style::default().fg(if places > 0 {
                ore::emerald()
            } else {
                ore::redstone()
            }),
        ),
    }
}

/// Daily villagers, as bars rather than a seven-character sparkline: the panel
/// is wide, so the days may as well be readable.
fn trend_panel(dash: &Dash, width: u16) -> Paragraph<'static> {
    let inner = width.saturating_sub(2) as usize;
    let mut lines = if dash.daily.len() > 1 {
        daily_chart(&dash.daily, inner, 3)
    } else {
        vec![Line::from(Span::styled(
            "  not enough days yet",
            Style::default().fg(ore::stone()),
        ))]
    };

    if let Some(caption) = trend_caption(&dash.daily, inner) {
        lines.push(Line::from(Span::styled(
            caption,
            Style::default().fg(theme::fade(theme::sage(), 0.2)),
        )));
    }

    Paragraph::new(lines).block(framed("DAILY USERS", "7", ore::grass()))
}

/// The line under the bars: how many days are drawn, and the tallest of them.
/// There is nothing to say before the first day's numbers arrive.
fn trend_caption(daily: &[f64], inner: usize) -> Option<String> {
    if daily.is_empty() {
        return None;
    }

    let shown = visible_days(inner, daily.len());
    let peak = daily[daily.len() - shown..]
        .iter()
        .cloned()
        .fold(0.0_f64, f64::max);
    Some(if shown < daily.len() {
        format!(
            "  last {} of {} days · peak {}",
            shown,
            daily.len(),
            commas(peak)
        )
    } else {
        format!("  {shown} days · peak {}", commas(peak))
    })
}

/// How many days a panel this wide can draw, at one column per day minimum.
fn visible_days(width: usize, total: usize) -> usize {
    total.min(width.saturating_sub(2).max(1)).max(1)
}

/// A column chart `rows` tall. Each day gets as many columns as the width
/// allows, and the eighth-height blocks give each bar eight times the vertical
/// resolution of the row it ends in.
fn daily_chart(values: &[f64], width: usize, rows: usize) -> Vec<Line<'static>> {
    // A narrow panel can't hold a bar per day; it shows the most recent ones
    // rather than squeezing every day into nothing.
    let days = visible_days(width, values.len());
    let values = &values[values.len() - days..];
    let peak = values.iter().cloned().fold(f64::MIN, f64::max).max(1.0);

    // Bars are measured from zero: for a count, a truncated axis would turn a
    // quiet week into a dramatic one.
    //
    // The width has to hold `days` bars, the gaps between them and the indent,
    // or the last bar runs under the border.
    let available = width.saturating_sub(2);
    let gap = usize::from(available >= days * 3);
    let bar = (available.saturating_sub(gap * (days - 1)) / days).clamp(1, 8);

    (0..rows)
        .map(|row| {
            // Row 0 is the top of the chart.
            let floor = (rows - row - 1) as f64;
            let mut spans = vec![Span::raw("  ")];
            for (i, value) in values.iter().enumerate() {
                let height = value / peak * rows as f64 - floor;
                let glyph = if height >= 1.0 {
                    glyph::FULL
                } else if height <= 0.0 {
                    ' '
                } else {
                    glyph::SPARK[((height * 8.0).ceil() as usize).clamp(1, 8) - 1]
                };
                // The latest day is the one people look for, so it is lit.
                let color = if i + 1 == values.len() {
                    theme::bright()
                } else {
                    theme::mix(
                        theme::accent_deep(),
                        ore::grass(),
                        i as f64 / values.len() as f64,
                    )
                };
                spans.push(Span::styled(
                    glyph.to_string().repeat(bar),
                    Style::default().fg(color),
                ));
                if i + 1 < values.len() {
                    spans.push(Span::raw(" ".repeat(gap)));
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// A very rough world, one `#` per land cell: 60 columns of longitude by 12
/// rows spanning 78°N to -57°S, which is where the populated land is. The
/// blank polar rows are left out so a short panel doesn't spend half its
/// height on empty ocean.
const WORLD: [&str; 12] = [
    "    ################   ####      ########################## ",
    "   #################   ###   ############################## ",
    "    ################        ######## ###################### ",
    "     ##############         ####### ######################  ",
    "       ##########          #############################    ",
    "            #####         ############     ###  ######      ",
    "                 #####     ###########           #####      ",
    "                 #######    #########            #######    ",
    "                 #######     #######              #######   ",
    "                  #####       #####               ######    ",
    "                   ###                                  ##  ",
    "                   ##                                       ",
];
/// Latitudes the template's first and last rows sit at.
const WORLD_TOP: f64 = 78.0;
const WORLD_BOTTOM: f64 = -57.0;

/// Approximate centroids for the countries GA reports most often. A country
/// missing from here still shows in the panel's caption — it just doesn't get
/// a dot.
const PLACES: [(&str, f64, f64); 46] = [
    ("United States", 39.0, -98.0),
    ("Canada", 56.0, -106.0),
    ("Mexico", 23.0, -102.0),
    ("Brazil", -10.0, -55.0),
    ("Argentina", -34.0, -64.0),
    ("Chile", -33.0, -71.0),
    ("Colombia", 4.0, -73.0),
    ("Peru", -10.0, -76.0),
    ("United Kingdom", 54.0, -2.0),
    ("Ireland", 53.0, -8.0),
    ("France", 46.0, 2.0),
    ("Spain", 40.0, -4.0),
    ("Portugal", 39.0, -8.0),
    ("Germany", 51.0, 10.0),
    ("Netherlands", 52.0, 5.0),
    ("Belgium", 51.0, 4.0),
    ("Switzerland", 47.0, 8.0),
    ("Austria", 47.0, 14.0),
    ("Italy", 42.0, 12.0),
    ("Poland", 52.0, 19.0),
    ("Czechia", 50.0, 15.0),
    ("Sweden", 62.0, 15.0),
    ("Norway", 61.0, 8.0),
    ("Denmark", 56.0, 10.0),
    ("Finland", 64.0, 26.0),
    ("Ukraine", 49.0, 32.0),
    ("Romania", 46.0, 25.0),
    ("Greece", 39.0, 22.0),
    ("Turkey", 39.0, 35.0),
    ("Russia", 60.0, 90.0),
    ("Israel", 31.0, 35.0),
    ("United Arab Emirates", 24.0, 54.0),
    ("Saudi Arabia", 24.0, 45.0),
    ("Egypt", 27.0, 30.0),
    ("Nigeria", 10.0, 8.0),
    ("Kenya", 0.0, 38.0),
    ("South Africa", -29.0, 24.0),
    ("India", 21.0, 78.0),
    ("Pakistan", 30.0, 70.0),
    ("Bangladesh", 24.0, 90.0),
    ("China", 35.0, 105.0),
    ("Japan", 36.0, 138.0),
    ("South Korea", 36.0, 128.0),
    ("Singapore", 1.0, 104.0),
    ("Indonesia", -2.0, 118.0),
    ("Australia", -25.0, 134.0),
];

/// Land nobody arrived from — the map's ground.
///
/// One quiet shade, not two. Mixing `█` and `▓` per cell was meant to read as
/// placed blocks, but in a single color they differ only in density, so the map
/// came out as dithered static that buried the realms lit on top of it. A light
/// shade gives the continents their silhouette back and leaves the full block
/// free to mean "somebody arrived from here".
const LAND: &str = "\u{2591}";

/// Where a country lands on a `cols` x `rows` grid, if we know it. The grid
/// covers the template's latitude band rather than the whole globe.
fn place(country: &str, cols: usize, rows: usize) -> Option<(usize, usize)> {
    let (_, lat, lon) = PLACES.iter().find(|(name, _, _)| *name == country)?;
    let span = WORLD_TOP - WORLD_BOTTOM;
    let x = ((lon + 180.0) / 360.0 * cols as f64) as usize;
    let y = ((WORLD_TOP - lat) / span * rows as f64).max(0.0) as usize;
    Some((x.min(cols - 1), y.min(rows - 1)))
}

/// Nudges a dot onto the nearest land cell. Centroids are approximate and the
/// map is coarse, so without this a country can end up a cell out to sea.
fn snap(land: &[Vec<bool>], x: usize, y: usize) -> (usize, usize) {
    if land[y][x] {
        return (x, y);
    }
    let mut best = None;
    for (dy, row) in land.iter().enumerate() {
        for (dx, is_land) in row.iter().enumerate() {
            if !is_land {
                continue;
            }
            let distance = (dx as i64 - x as i64).pow(2) + 2 * (dy as i64 - y as i64).pow(2);
            if distance <= 8 && best.map(|(d, _, _)| distance < d).unwrap_or(true) {
                best = Some((distance, dx, dy));
            }
        }
    }
    best.map(|(_, dx, dy)| (dx, dy)).unwrap_or((x, y))
}

/// Who is online, on a map. Land is drawn in shadow and the countries with
/// players are lit on top of it, brightest where the most are.
fn map_panel(dash: &Dash, width: u16, height: u16) -> Paragraph<'static> {
    let cols = (width.saturating_sub(4) as usize).clamp(20, WORLD[0].len());
    let rows = (height.saturating_sub(3) as usize).clamp(4, WORLD.len());
    let phase = dash.phase();

    // Sample the template down to the panel's size. A cell is land if any
    // template cell it covers is land, so shrinking thins the coasts rather
    // than punching holes in them.
    let land: Vec<Vec<bool>> = (0..rows)
        .map(|y| {
            let from = y * WORLD.len() / rows;
            let to = ((y + 1) * WORLD.len() / rows).max(from + 1);
            (0..cols)
                .map(|x| {
                    let left = x * WORLD[0].len() / cols;
                    let right = ((x + 1) * WORLD[0].len() / cols).max(left + 1);
                    WORLD[from..to]
                        .iter()
                        .any(|row| row.as_bytes()[left..right.min(row.len())].contains(&b'#'))
                })
                .collect()
        })
        .collect();

    let mut grid: Vec<Vec<Option<Color>>> = land
        .iter()
        .map(|row| {
            row.iter()
                .map(|is_land| is_land.then(theme::shadow))
                .collect()
        })
        .collect();

    // Base layer: where the period's users came from, weighted by how many.
    let peak = dash
        .realms
        .iter()
        .map(|(_, users)| *users)
        .fold(1.0_f64, f64::max);

    for (country, users) in &dash.realms {
        if *users <= 0.0 {
            continue;
        }
        let Some((x, y)) = place(country, cols, rows) else {
            continue;
        };
        let (x, y) = snap(&land, x, y);
        // Square root, or one dominant country flattens every other realm to
        // the dimmest shade on the map.
        let weight = (users / peak).clamp(0.0, 1.0).sqrt();
        grid[y][x] = Some(theme::mix(theme::accent_deep(), ore::grass(), weight));
    }

    // Overlay: countries with somebody on the site this minute, breathing on
    // the same clock as the rest of the dashboard.
    let breath = (phase * 2.2).sin() * 0.5 + 0.5;
    for (country, users) in &dash.live_realms {
        if *users <= 0.0 {
            continue;
        }
        let Some((x, y)) = place(country, cols, rows) else {
            continue;
        };
        let (x, y) = snap(&land, x, y);
        grid[y][x] = Some(theme::mix(theme::accent(), theme::bright(), breath));
    }

    let mut lines: Vec<Line> = grid
        .into_iter()
        .map(|row| {
            let mut spans = vec![Span::raw("  ")];
            for cell in row {
                spans.push(match cell {
                    // Open water.
                    None => Span::raw(" "),
                    // Land nobody arrived from — the ground the realms sit on.
                    Some(color) if color == theme::shadow() => {
                        Span::styled(LAND, Style::default().fg(color))
                    }
                    // A realm with traffic reads as an ore seam in that ground:
                    // the full block, lit by the ore's own color.
                    Some(color) => Span::styled(
                        glyph::FULL.to_string(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                });
            }
            Line::from(spans)
        })
        .collect();

    // The countries themselves, since a dot on a map this rough is not a label.
    // As many as the width holds, rather than a fixed three and an ellipsis.
    let mut top: Vec<&(String, f64)> = dash.realms.iter().collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));

    let online = dash
        .live_realms
        .iter()
        .filter(|(_, users)| *users > 0.0)
        .count();
    let suffix = format!(" · {online} online now");
    let budget = (width as usize).saturating_sub(4 + suffix.chars().count());

    let mut named = String::new();
    for (name, users) in top.iter().take(4) {
        let piece = format!("{name} {}", commas(*users));
        let candidate = if named.is_empty() {
            piece
        } else {
            format!("{named} · {piece}")
        };
        if candidate.chars().count() > budget {
            break;
        }
        named = candidate;
    }
    if named.is_empty() {
        named = "no realms in this window".to_string();
    }

    lines.push(Line::from(Span::styled(
        format!("  {named}{suffix}"),
        Style::default().fg(theme::fade(theme::sage(), 0.2)),
    )));

    Paragraph::new(lines).block(framed("COUNTRIES", "3", ore::lapis()))
}

/// What sits under the vitals in the left-hand column.
enum Stack {
    Map,
    Events,
    Vitals,
}

/// Which panel occupies a slot in the right-hand column.
enum Column {
    Live,
    Chunks,
    RealmsRanked,
    Trend,
}

/// Events per day, this period drawn over the last one.
///
/// A line chart rather than the ranked bars this replaced: the question the
/// panel answers is "are events climbing or falling", which a per-event
/// leaderboard cannot show at all — it ranks names, and the ranking barely
/// moves. Both periods share one y scale, and the current one is drawn second so
/// it sits on top where they cross.
fn events_panel(dash: &Dash) -> Chart<'_> {
    let trend = &dash.events;
    // An empty or flat series would collapse the y axis onto a single row.
    let peak = trend.peak.max(1.0);
    let last = trend.current.len().saturating_sub(1).max(1) as f64;

    let datasets = vec![
        Dataset::default()
            .marker(symbols::Marker::HalfBlock)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(theme::accent_deep()))
            .data(&trend.previous),
        Dataset::default()
            .marker(symbols::Marker::HalfBlock)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(theme::accent()))
            .data(&trend.current),
    ];

    // The headline rides on the border, where the panel has room for it.
    let headline = Line::from(vec![
        Span::styled(
            format!(" {} ", commas(trend.total)),
            Style::default()
                .fg(theme::bright())
                .add_modifier(Modifier::BOLD),
        ),
        delta_span(trend.total, trend.total_previous, false),
        Span::raw(" "),
    ])
    .right_aligned();

    // The legend rides the bottom border rather than sitting inside the plot:
    // ratatui hides its own legend once the panel is short, and the left column
    // never gives this one the rows it wants.
    let legend = Line::from(vec![
        Span::styled("\u{2501}\u{2501} ", Style::default().fg(theme::accent())),
        Span::styled(
            format!("last {} days  ", dash.days),
            Style::default().fg(theme::sage()),
        ),
        Span::styled(
            "\u{2501}\u{2501} ",
            Style::default().fg(theme::accent_deep()),
        ),
        Span::styled("previous ", Style::default().fg(theme::sage())),
    ])
    .right_aligned();

    Chart::new(datasets)
        .block(
            framed("EVENTS", "1", ore::xp())
                .title_top(headline)
                .title_bottom(legend),
        )
        .legend_position(None)
        .x_axis(
            Axis::default()
                .style(Style::default().fg(theme::shadow()))
                .bounds([0.0, last])
                .labels(axis_days(&trend.days)),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(theme::shadow()))
                .bounds([0.0, peak])
                .labels(axis_counts(peak)),
        )
}

/// First, middle and last day of the period. Every day would not fit, and
/// ratatui spreads whatever it is given evenly across the axis.
fn axis_days(days: &[String]) -> Vec<Line<'static>> {
    let label = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(theme::sage()),
        ))
    };
    match days.len() {
        0 => Vec::new(),
        1 => vec![label(&days[0])],
        n => vec![label(&days[0]), label(&days[n / 2]), label(&days[n - 1])],
    }
}

/// Zero, half and full scale up the y axis.
fn axis_counts(peak: f64) -> Vec<Line<'static>> {
    [0.0, peak / 2.0, peak]
        .iter()
        .map(|value| {
            Line::from(Span::styled(
                commas(value.round()),
                Style::default().fg(theme::sage()),
            ))
        })
        .collect()
}

/// The realtime panel: the count, an htop-style meter, the scrolling trace,
/// and the arrivals behind the last few changes.
fn live_panel(dash: &Dash, width: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    let breath = (phase * 2.2).sin() * 0.5 + 0.5;

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("  {} ", glyph::PULSE[((breath * 2.99) as usize).min(2)]),
                Style::default().fg(theme::mix(theme::accent_deep(), theme::bright(), breath)),
            ),
            Span::styled(
                commas(dash.live.shown.round()),
                Style::default()
                    .fg(theme::mix(theme::accent(), theme::bright(), breath))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  players online", Style::default().fg(ore::stone())),
        ]),
        Line::from(Span::styled(
            truncate(
                &format!(
                    "  active last 30 min · polled {}s",
                    dash.live_every.as_secs(),
                ),
                width.saturating_sub(3) as usize,
            ),
            Style::default().fg(theme::fade(theme::sage(), 0.3)),
        )),
        Line::from(""),
    ];

    // Event feed — recent arrivals and departures.
    //
    // Always FEED_ROWS tall. Anything not lit is drawn as unlit cells, so the
    // field is the same size whether six things just happened or nothing has.
    let mut feed_rows = 0usize;
    if dash.feed.is_empty() {
        lines.push(Line::from(Span::styled(
            "  quiet out there",
            Style::default().fg(theme::fade(theme::sage(), 0.3)),
        )));
        feed_rows += 1;
    } else {
        // The feed reads as a small LCD: one hue, hierarchy spent entirely on
        // brightness, and every row sitting on a field of unlit cells.
        //
        // Monochrome is the point. The old rows coloured rising green and
        // falling purple, which put direction in the one channel an LCD does
        // not have — so direction moves to the glyph, where ▲/▼ carries it even
        // on a terminal with no colour at all. The panel's own hue comes from
        // the palette, so each theme lights its screen its own way.
        let lit = theme::accent();
        for entry in dash.feed.iter().take(6) {
            let age = entry.at.elapsed().as_secs_f64() / FEED_TTL.as_secs_f64();
            let rising = entry.delta > 0.0;
            let time_ago = entry.at.elapsed().as_secs();
            let time_str = if time_ago < 60 {
                format!("{}s", time_ago)
            } else {
                format!("{}m", time_ago / 60)
            };

            let glyph_cell = format!("  {} ", if rising { glyph::UP } else { glyph::DOWN });
            let delta_cell = format!("{:>4} ", format!("{:+}", entry.delta as i64));
            let label_cell = if rising { "spawned in" } else { "wandered off" }.to_string();
            let time_cell = format!("  {}", time_str);

            // Unlit cells fill the rest of the row, so the field is visible
            // where nothing is lit — that texture is what separates a dot
            // matrix from a plain list. Ages out with the row it belongs to.
            let used = glyph_cell.chars().count()
                + delta_cell.chars().count()
                + label_cell.chars().count()
                + time_cell.chars().count();
            let unlit: String = std::iter::repeat(glyph::UNLIT)
                .take((width as usize).saturating_sub(used + 3))
                .collect();

            lines.push(Line::from(vec![
                Span::styled(glyph_cell, Style::default().fg(theme::fade(lit, age))),
                Span::styled(
                    delta_cell,
                    Style::default()
                        .fg(theme::fade(lit, age))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label_cell,
                    // A dimmer run of the same hue, never a second colour.
                    Style::default().fg(theme::fade(lit, (age + 0.45).min(1.0))),
                ),
                Span::styled(
                    time_cell,
                    Style::default().fg(theme::fade(lit, (age + 0.65).min(1.0))),
                ),
                Span::styled(
                    unlit,
                    Style::default().fg(theme::fade(lit, (age + 0.86).min(1.0))),
                ),
            ]));
            feed_rows += 1;
        }
    }

    // The dark rest of the screen. Dimmer than the faintest live row, so it
    // reads as field rather than as an event that has nearly faded out.
    let dark: String = std::iter::repeat(glyph::UNLIT)
        .take((width as usize).saturating_sub(5))
        .collect();
    for _ in feed_rows..FEED_ROWS {
        lines.push(Line::from(Span::styled(
            format!("  {dark}"),
            Style::default().fg(theme::fade(theme::accent(), 0.93)),
        )));
    }

    Paragraph::new(lines).block(framed("RIGHT NOW", "2", ore::xp()))
}

fn pages_panel(dash: &Dash, width: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    let mut lines: Vec<Line> = Vec::new();

    // Row shape is `   <bar>  <views> <share>`, so the bar gets whatever the
    // indent, the gaps and the two number columns leave — otherwise they run
    // under the border on an 80-column terminal.
    let inner = width.saturating_sub(2) as usize;
    let cells = inner
        .saturating_sub(3 + 2 + VIEWS_COLUMN + SHARE_COLUMN)
        .clamp(4, 20);
    // The label shares its line with the movement marker.
    let label_cells = inner.saturating_sub(4 + MOVED_COLUMN);

    // Share is of *all* page views for the period, not of the eight rows shown,
    // which is why it comes from the headline metric rather than these rows.
    let views_total = OVERVIEW
        .iter()
        .position(|metric| metric.api == "screenPageViews")
        .and_then(|i| dash.metrics.get(i))
        .map(|row| row.value.target)
        .unwrap_or(0.0);

    if dash.pages.is_empty() {
        lines.push(Line::from(Span::styled(
            "no data in this window",
            Style::default().fg(ore::stone()),
        )));
    }

    for (i, row) in dash.pages.iter().enumerate() {
        let color = theme::ramp(i);
        let label: String = row.path.chars().take(label_cells).collect();
        let (ore, ore_color) = tier(i);

        let mut heading = vec![
            Span::styled(
                format!("{ore} "),
                Style::default().fg(ore_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{label:<label_cells$}"), Style::default().fg(color)),
        ];
        heading.push(moved_span(row.moved));
        lines.push(Line::from(heading));

        let mut spans = bar_spans(
            row.frac.shown,
            cells,
            glyph::FULL,
            color,
            phase,
            row.frac.moving(),
            3,
        );
        spans.push(Span::styled(
            format!(
                "  {:>width$}",
                commas(row.views.shown.round()),
                width = VIEWS_COLUMN
            ),
            Style::default()
                .fg(theme::fg())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            if views_total > 0.0 {
                format!(
                    "{:>width$}",
                    format!("{:.0}%", row.views.shown / views_total * 100.0),
                    width = SHARE_COLUMN
                )
            } else {
                " ".repeat(SHARE_COLUMN)
            },
            Style::default().fg(ore::stone()),
        ));
        lines.push(Line::from(spans));
    }

    Paragraph::new(lines).block(framed("TOP PAGES", "4", ore::copper()))
}

fn realms_ranked_panel(dash: &Dash, width: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    let inner = width.saturating_sub(2) as usize;
    let cells = inner.saturating_sub(3 + 2 + VIEWS_COLUMN).clamp(4, 20);
    let label_cells = inner.saturating_sub(4 + MOVED_COLUMN);

    let peak = dash.realms.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);

    let mut sorted: Vec<&(String, f64)> = dash.realms.iter().collect();
    sorted.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut lines: Vec<Line> = Vec::new();

    if sorted.is_empty() {
        lines.push(Line::from(Span::styled(
            "no realm data in this window",
            Style::default().fg(ore::stone()),
        )));
    }

    for (i, (country, count)) in sorted.iter().take(8).enumerate() {
        let color = theme::ramp(i);
        let label: String = country.chars().take(label_cells).collect();
        let (ore_badge, ore_color) = tier(i);
        let frac = if peak > 0.0 { *count / peak } else { 0.0 };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{ore_badge} "),
                Style::default().fg(ore_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{label:<label_cells$}"), Style::default().fg(color)),
            Span::raw(" ".repeat(MOVED_COLUMN)),
        ]));

        let mut spans = bar_spans(frac, cells, glyph::FULL, color, phase, false, 3);
        spans.push(Span::styled(
            format!("  {:>width$}", commas(*count), width = VIEWS_COLUMN),
            Style::default()
                .fg(theme::fg())
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(spans));
    }

    Paragraph::new(lines).block(framed("TOP COUNTRIES", "6", ore::lapis()))
}

fn footer(dash: &Dash) -> Paragraph<'static> {
    if let Some(err) = &dash.error {
        return Paragraph::new(Line::from(Span::styled(
            format!(" ⚠ {} (showing last good data)", truncate(err, 70)),
            Style::default().fg(ore::redstone()),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ore::redstone()))
                .style(Style::default().bg(theme::bg_lift())),
        );
    }

    // Hotbar-style footer: each key lives in its own slot, separated by
    // netherite walls — like the nine-slot bar at the bottom of the screen.
    let wall = || Span::styled(" │ ", Style::default().fg(ore::netherite()));
    let slot = |k: &str, label: &str| -> Vec<Span<'static>> {
        vec![
            Span::styled(
                format!("[{k}]"),
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(label.to_string(), Style::default().fg(ore::stone())),
        ]
    };

    let breath = (dash.phase() * 2.2).sin() * 0.5 + 0.5;
    let pulse = if dash.in_flight > 0 {
        Span::styled(
            format!("{}", spinner(dash.phase())),
            Style::default().fg(theme::bright()),
        )
    } else {
        Span::styled(
            format!("{}", glyph::PULSE[((breath * 2.99) as usize).min(2)]),
            Style::default().fg(theme::mix(theme::accent_deep(), theme::bright(), breath)),
        )
    };

    let mut spans = vec![Span::raw(" ")];

    // Each key is a hotbar slot: [key]label separated by netherite walls.
    for (k, label) in [("q", "quit"), ("r", "rebuild"), ("?", "help")] {
        spans.push(wall());
        spans.extend(slot(k, label));
    }
    // Theme slot — the pack name is the item.
    spans.push(wall());
    spans.extend(slot("t", ""));
    spans.push(Span::styled(
        theme::palette().name.to_string(),
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    ));

    // Live indicator in its own slot.
    spans.push(wall());
    spans.push(pulse);
    spans.push(Span::styled(
        " live",
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(" "));

    // Timestamp behind a final wall.
    spans.push(wall());
    spans.push(Span::styled(
        format!("· updated {}", dash.updated),
        Style::default().fg(ore::stone()),
    ));

    Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ore::netherite()))
            .style(Style::default().bg(theme::bg_lift())),
    )
}

// ------------------------------------------------------------- primitives ---

/// A block bar with two pieces of motion: a partial leading cell drawn as one
/// of Minecraft's break stages, and a brighter block sweeping along the mined
/// section, so a bar reads as being dug rather than merely drawn.
///
/// The sweep never stops. While the bar is filling it runs bright and fast, and
/// once the value settles it drops to a slow, barely-there shimmer — the screen
/// is never completely still, which is what makes a dashboard look connected
/// rather than crashed.
fn bar_spans(
    frac: f64,
    cells: usize,
    block: char,
    color: Color,
    phase: f64,
    active: bool,
    indent: usize,
) -> Vec<Span<'static>> {
    let frac = frac.clamp(0.0, 1.0);
    let exact = frac * cells as f64;
    let filled = (exact.floor() as usize).min(cells);
    let remainder = exact - filled as f64;

    // Mute toward the shadow palette — Minecraft's textures are noisy and
    // desaturated, not neon-bright.
    let color = theme::mix(color, theme::shadow(), 0.30);

    let mut spans = vec![Span::raw(" ".repeat(indent))];

    if filled > 0 {
        let (speed, lift) = if active { (12.0, 0.6) } else { (3.0, 0.22) };
        let head = ((phase * speed) as usize) % filled;
        let mut highlight = Style::default().fg(theme::brighten(color, lift));
        if active {
            highlight = highlight.add_modifier(Modifier::BOLD);
        }

        spans.push(Span::styled(
            block.to_string().repeat(head),
            Style::default().fg(color),
        ));
        spans.push(Span::styled(block.to_string(), highlight));
        spans.push(Span::styled(
            block.to_string().repeat(filled - head - 1),
            Style::default().fg(color),
        ));
    }

    let mut used = filled;
    if used < cells && remainder > 0.1 {
        let stage = if remainder > 0.66 {
            glyph::PARTIAL
        } else if remainder > 0.33 {
            glyph::CRACKED
        } else {
            glyph::EMPTY
        };
        spans.push(Span::styled(
            stage.to_string(),
            Style::default().fg(theme::mix(color, theme::shadow(), 0.35)),
        ));
        used += 1;
    }

    if used < cells {
        spans.push(Span::styled(
            glyph::EMPTY.to_string().repeat(cells - used),
            Style::default().fg(theme::shadow()),
        ));
    }

    spans
}

fn spinner(phase: f64) -> char {
    glyph::SPINNER[((phase * 12.0) as usize) % glyph::SPINNER.len()]
}

fn delta_span(current: f64, previous: f64, lower_is_better: bool) -> Span<'static> {
    if previous <= 0.0 {
        return Span::raw("");
    }
    let change = (current - previous) / previous * 100.0;
    if !change.is_finite() || change.abs() < 0.5 {
        return Span::styled("— flat", Style::default().fg(ore::stone()));
    }
    let rising = change > 0.0;
    let good = rising != lower_is_better;
    Span::styled(
        format!(
            "{}{:.0}%",
            if rising { glyph::UP } else { glyph::DOWN },
            change.abs()
        ),
        Style::default().fg(if good {
            ore::emerald()
        } else {
            ore::redstone()
        }),
    )
}

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

    /// A dashboard on the demo numbers, settled so nothing is mid-ease.
    fn settled_demo() -> Dash {
        let mut synthetic = Synthetic::new();
        let snapshot = synthetic.report(&mut rand::thread_rng());
        let mut dash = Dash::new(
            "test".to_string(),
            7,
            snapshot,
            128.0,
            Vec::new(),
            Duration::from_secs(30),
            Duration::from_secs(5),
        );
        for _ in 0..80 {
            dash.step(FRAME.as_secs_f64());
        }
        dash
    }

    /// Renders a widget into a fixed grid and hands back the rows as text.
    fn rendered<W: ratatui::widgets::Widget>(width: u16, height: u16, widget: W) -> Vec<String> {
        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(widget, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The chart is the panel's whole point, so the two things that make it
    /// readable — a drawn line and the axis it is read against — have to survive
    /// every width the left column can hand it.
    ///
    /// It also has to draw in glyphs the site's font actually carries. The
    /// braille markers this started out with looked best in a terminal, but the
    /// self-hosted JetBrains Mono subset has none of U+2800..U+28FF, so every
    /// plotted cell fell back to a face with a different advance and dragged the
    /// rest of its row out of the grid.
    #[test]
    fn the_events_chart_draws_both_periods_against_an_axis() {
        let dash = settled_demo();
        assert_eq!(dash.events.current.len(), 7);
        assert_eq!(dash.events.previous.len(), 7);

        for width in 40..=120u16 {
            let rows = rendered(width, EVENTS_ROWS, events_panel(&dash));
            let panel = rows.join("\n");

            let plotted = panel
                .chars()
                .filter(|c| matches!(c, '\u{2588}' | '\u{2584}' | '\u{2580}'))
                .count();
            assert!(plotted > 20, "width {width}: only {plotted} plotted cells");

            assert!(
                !panel
                    .chars()
                    .any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)),
                "width {width}: braille has no glyph in the site's font"
            );

            // The scale the lines are read against.
            let peak = commas(dash.events.peak.round());
            assert!(
                panel.contains(&peak),
                "width {width}: y axis lost its {peak} label"
            );
            assert!(
                panel.contains(&dash.events.days[0]),
                "width {width}: x axis lost its first day"
            );
        }
    }

    #[test]
    fn eased_settles_exactly_on_its_target() {
        let mut eased = Eased::new(1200.0);
        // Two seconds at the frame budget; anything still drifting after that
        // would leave the mining edge flickering forever.
        for _ in 0..40 {
            eased.step(FRAME.as_secs_f64());
        }
        assert_eq!(eased.shown, 1200.0);
        assert!(!eased.moving());
    }

    #[test]
    fn a_property_without_settings_falls_back_to_the_defaults() {
        let defaults = Settings {
            days: 7,
            refresh: 30,
            live_refresh: 3,
        };
        let bare = Property {
            id: "222".into(),
            ..Property::default()
        };
        let tuned = Property {
            id: "111".into(),
            days: Some(28),
            refresh: Some(120),
            ..Property::default()
        };

        let a = defaults.for_property(&tuned);
        assert_eq!((a.days, a.refresh, a.live_refresh), (28, 120, 3));

        // Switching to a bare property must land on the defaults, not inherit
        // the 28 days the previous property asked for.
        let b = defaults.for_property(&bare);
        assert_eq!((b.days, b.refresh, b.live_refresh), (7, 30, 3));
    }

    #[test]
    fn eased_is_framerate_independent() {
        let (mut fast, mut slow) = (Eased::new(100.0), Eased::new(100.0));
        for _ in 0..20 {
            fast.step(0.05);
        }
        for _ in 0..5 {
            slow.step(0.2);
        }
        // One second of motion either way lands in the same place.
        assert!((fast.shown - slow.shown).abs() < 0.5);
    }

    #[test]
    fn bars_always_fill_exactly_their_cells() {
        for step in 0..=20 {
            let frac = step as f64 / 20.0;
            for active in [false, true] {
                let width: usize = bar_spans(frac, 16, glyph::FULL, ore::grass(), 1.7, active, 2)
                    .iter()
                    .skip(1) // the indent
                    .map(|span| span.content.chars().count())
                    .sum();
                assert_eq!(width, 16, "frac {frac}, active {active}");
            }
        }
    }

    #[test]
    fn the_sweep_keeps_moving_after_a_bar_settles() {
        // `head` is the length of the span before the highlight, so a moving
        // sweep shows up as that span changing length over time.
        let head = |phase: f64| {
            bar_spans(1.0, 20, glyph::FULL, ore::grass(), phase, false, 0)[1]
                .content
                .chars()
                .count()
        };
        let settled: Vec<usize> = (0..12).map(|i| head(i as f64 * 0.25)).collect();
        assert!(
            settled.windows(2).any(|w| w[0] != w[1]),
            "a settled bar stopped animating: {settled:?}"
        );
    }

    #[test]
    fn a_small_terminal_gets_the_notice_not_a_broken_dashboard() {
        // Both numbers are named, and the short one is what the reader needs.
        let text = render_to_string(too_small(Rect::new(0, 0, 72, 18)));
        assert!(text.contains("72"), "missing actual width: {text:?}");
        assert!(text.contains("18"), "missing actual height: {text:?}");
        assert!(
            text.contains(&MIN_COLS.to_string()) && text.contains(&MIN_ROWS.to_string()),
            "missing what it needs: {text:?}"
        );
    }

    #[test]
    fn events_outranks_vitals_when_the_column_is_short() {
        // The allocation the left column runs, over every height: events takes
        // its box first, vitals only gets one from what is left. A column with
        // room for exactly one of the two must spend it on events.
        for height in 0..=60u16 {
            let mut budget = height;
            let events = budget >= EVENTS_ROWS;
            if events {
                budget -= EVENTS_ROWS;
            }
            let vitals = budget >= VITALS_ROWS + u16::from(events);

            if height >= EVENTS_ROWS {
                assert!(events, "height {height}: events dropped while it fitted");
            }
            if vitals && events {
                assert!(
                    height >= EVENTS_ROWS + 1 + VITALS_ROWS,
                    "height {height}: both boxes claimed without the rows for both"
                );
            }
            // The point of the order: one box's worth of rows goes to events.
            if height >= EVENTS_ROWS && height < EVENTS_ROWS + 1 + VITALS_ROWS {
                assert!(!vitals, "height {height}: vitals took rows events needed");
            }
        }
    }

    #[test]
    fn the_supporter_box_states_which_side_you_are_on() {
        // The ask and the thank-you are the same box, and neither state is
        // allowed to be silent — this is the line that pays for the rest.
        let mut dash = capture_dash();

        dash.supporter = false;
        let text = render_to_string(supporter_box(&dash));
        assert!(text.contains("craft subscribe"), "no ask: {text:?}");
        assert!(
            text.contains("not an Anacrafter yet"),
            "no status: {text:?}"
        );

        dash.supporter = true;
        let text = render_to_string(supporter_box(&dash));
        assert!(text.contains("ANACRAFTER"), "no status: {text:?}");
        assert!(!text.contains("craft subscribe"), "still asking: {text:?}");
    }

    /// A paragraph's Debug repr, which embeds every span's content — enough to
    /// assert on what a widget says without standing up a terminal backend.
    /// Matches substrings only; it is not a layout assertion.
    fn render_to_string(p: Paragraph<'static>) -> String {
        format!("{p:?}")
    }

    #[test]
    fn header_realms_are_dropped_whole_never_sliced() {
        let realms: Vec<(String, f64)> = vec![
            ("United States".into(), 44.0),
            ("India".into(), 18.0),
            ("Germany".into(), 12.0),
            ("United Kingdom".into(), 11.0),
            ("Brazil".into(), 8.0),
        ];
        for budget in 0..=64 {
            let chips = realm_chips(&realms, budget);
            let drawn: usize = chips
                .iter()
                .map(|(sep, chip)| sep.chars().count() + chip.chars().count())
                .sum();
            assert!(drawn <= budget, "budget {budget}: drew {drawn}");
            for (_, chip) in &chips {
                let count = chip.split(':').nth(1);
                assert!(
                    count.is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())),
                    "budget {budget}: truncated chip {chip:?}"
                );
            }
        }
        // Given room for everything, nothing is dropped.
        assert_eq!(realm_chips(&realms, 200).len(), 5);

        // The two "United ..." realms must not collapse onto the same label.
        let labels: Vec<String> = realm_chips(&realms, 200)
            .into_iter()
            .map(|(_, chip)| chip.split(':').next().unwrap().to_string())
            .collect();
        let unique: std::collections::HashSet<&String> = labels.iter().collect();
        assert_eq!(
            unique.len(),
            labels.len(),
            "ambiguous realm labels: {labels:?}"
        );
    }

    #[test]
    fn realm_abbreviations_distinguish_similar_names() {
        assert_eq!(realm_abbrev("United States"), "US");
        assert_eq!(realm_abbrev("United Kingdom"), "UK");
        assert_eq!(realm_abbrev("United Arab Emirates"), "UAE");
        assert_eq!(realm_abbrev("Bosnia and Herzegovina"), "BH");
        assert_eq!(realm_abbrev("India"), "Ind");
        assert_eq!(realm_abbrev("Germany"), "Ger");
    }

    #[test]
    fn the_map_separates_its_realms_from_its_land() {
        let dash = settled_demo();
        let rows = rendered(74, MAP_ROWS, map_panel(&dash, 74, MAP_ROWS));
        // The map's own rows, without the border and the caption under it.
        let map = &rows[1..rows.len() - 2];

        let land: usize = map.iter().map(|row| row.matches(LAND).count()).sum();
        let realms: usize = map.iter().map(|row| row.matches(glyph::FULL).count()).sum();

        // Land has to be one shade and the realms another, or the lit cells are
        // lost in the ground they sit on — which is what the two-shade terrain
        // this replaced did to them.
        assert_ne!(LAND, glyph::FULL.to_string());
        assert!(land > 100, "expected a drawn landmass, got {land} cells");
        assert!(
            realms > 0 && realms < land,
            "{realms} lit realms against {land} land cells"
        );
    }

    #[test]
    fn the_trend_caption_waits_for_the_first_day() {
        // `visible_days` never returns zero, so an empty history used to slice
        // from behind the front of the vec and take the whole dashboard down.
        assert_eq!(trend_caption(&[], 40), None);
        assert!(trend_caption(&[12.0], 40).unwrap().contains("1 days"));
    }

    #[test]
    fn the_daily_chart_fits_its_panel() {
        for width in 12..=60 {
            for days in [2, 7, 14, 30] {
                let values: Vec<f64> = (0..days).map(|d| 100.0 + d as f64).collect();
                for line in daily_chart(&values, width, 3) {
                    let drawn: usize = line
                        .spans
                        .iter()
                        .map(|span| span.content.chars().count())
                        .sum();
                    assert!(drawn <= width, "width {width}, {days} days: drew {drawn}");
                }
            }
        }
    }
}
