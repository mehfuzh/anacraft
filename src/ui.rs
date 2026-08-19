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
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::config::Config;
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
/// What the vitals panel needs: a label, a bar and a gap for each headline
/// metric, the daily sparkline, and its borders.
const VITALS_ROWS: u16 = (OVERVIEW.len() as u16) * 3 + 2;
/// The daily chart's box: three rows of bars, a caption, and borders.
const TREND_ROWS: u16 = 3 + 1 + 2;
/// What the live panel needs before it is worth drawing: the count, its
/// caption, the meter, the graph and one line of feed, plus borders.
const LIVE_ROWS: u16 = 9;
/// Two chunks and borders. It takes the column's spare rows on top of this.
const CHUNKS_ROWS: u16 = 6;
/// Ranked realms: eight country rows with bars, plus borders.
const REALMS_RANKED_ROWS: u16 = 10;
/// The map's box: nine rows of world, a caption, and borders.
const MAP_ROWS: u16 = 12;
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

/// One report pass. The realtime number is not in here: it arrives on its own
/// cadence, and folding it in would make it as stale as the reports.
struct Snapshot {
    current: Vec<f64>,
    previous: Vec<f64>,
    daily: Vec<f64>,
    pages: Vec<(String, f64)>,
    realms: Vec<(String, f64)>,
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
    block: bool,
}

impl Panels {
    fn any(&self) -> bool {
        self.vitals || self.live || self.chunks || self.realms_ranked || self.trend || self.map
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
                block: true,
            },
            help: false,
            peak: live.max(1.0),
            started: Instant::now(),
            last_report: Instant::now(),
            report_every,
            live_every,
        };
        dash.apply_report(snapshot);
        // The constructor's own report shouldn't set every row flashing.
        for row in &mut dash.metrics {
            row.flash = None;
        }
        dash
    }

    fn apply_report(&mut self, snapshot: Snapshot) {
        self.apply_totals(snapshot.current, snapshot.previous);
        self.apply_trend(snapshot.daily);
        self.apply_pages(snapshot.pages);
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

/// The whole set at once, for the fetch that happens before the screen is
/// taken over. The three run concurrently, so this costs one round trip rather
/// than three.
async fn fetch_report(client: &Ga, property: &str, days: u32) -> Result<Snapshot> {
    let (totals, daily, pages, realms) = tokio::try_join!(
        fetch_totals(client, property, days),
        fetch_trend(client, property, days),
        fetch_pages(client, property, days),
        fetch_realms(client, property, days)
    )?;

    Ok(Snapshot {
        current: totals.0,
        previous: totals.1,
        daily,
        pages,
        realms,
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

/// The four requests a report pass is made of.
#[derive(Clone, Copy)]
enum Part {
    Totals,
    Trend,
    Pages,
    Realms,
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
                4
            }
            Source::Demo(synthetic) => {
                let snapshot = synthetic.lock().unwrap().report();
                let _ = tx.send(Update::Totals {
                    current: snapshot.current,
                    previous: snapshot.previous,
                });
                let _ = tx.send(Update::Trend(snapshot.daily));
                let _ = tx.send(Update::Pages(snapshot.pages));
                let _ = tx.send(Update::Realms(snapshot.realms));
                4
            }
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
                let (total, realms) = synthetic.lock().unwrap().live();
                let _ = tx.send(Update::Live { total, realms });
            }
        }
    }
}

// ------------------------------------------------------------------- loop ---

pub async fn run(property: &str, days: u32, refresh: u64, live_refresh: u64) -> Result<()> {
    let client = Arc::new(Ga::new()?);
    let title = Config::load()?
        .property_name
        .unwrap_or_else(|| format!("property {property}"));

    // Fetch before taking over the screen so auth/API errors print normally.
    let snapshot = fetch_report(&client, property, days).await?;
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
        days,
        snapshot,
        live,
        realms,
        refresh,
        live_refresh,
    )
    .await
}

/// The dashboard on synthetic data — no account, but the same code path, which
/// is what makes it usable for screenshots and for tuning the animation.
pub async fn run_demo(days: u32, refresh: u64, live_refresh: u64) -> Result<()> {
    let mut synthetic = Synthetic::new();
    let snapshot = synthetic.report();
    let (live, realms) = synthetic.live();

    let source = Source::Demo(std::sync::Mutex::new(synthetic));
    drive(
        source,
        "Redstone Labs (demo)".to_string(),
        days,
        snapshot,
        live,
        realms,
        refresh,
        live_refresh,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    source: Source,
    title: String,
    days: u32,
    snapshot: Snapshot,
    live: f64,
    realms: Vec<(String, f64)>,
    refresh: u64,
    live_refresh: u64,
) -> Result<()> {
    let mut dash = Dash::new(
        title,
        days,
        snapshot,
        live,
        realms,
        Duration::from_secs(refresh.max(5)),
        Duration::from_secs(live_refresh.max(LIVE_FLOOR)),
    );

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
    let result = event_loop(&mut terminal, &source, days, &mut dash).await;
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    // Persist the theme the user settled on — so the next launch starts there.
    if let Ok(mut cfg) = crate::config::Config::load() {
        cfg.theme = Some(theme::palette().name.to_string());
        let _ = cfg.save();
    }
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    source: &Source,
    days: u32,
    dash: &mut Dash,
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
            dash.in_flight = source.request_report(days, &tx);
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
                        KeyCode::Char('1') | KeyCode::Char('b') => {
                            dash.panels.block = !dash.panels.block
                        }
                        KeyCode::Char('2') | KeyCode::Char('l') => {
                            dash.panels.live = !dash.panels.live
                        }
                        KeyCode::Char('3') | KeyCode::Char('p') => {
                            dash.panels.chunks = !dash.panels.chunks
                        }
                        KeyCode::Char('4') | KeyCode::Char('d') => {
                            dash.panels.trend = !dash.panels.trend
                        }
                        KeyCode::Char('5') | KeyCode::Char('m') => {
                            dash.panels.map = !dash.panels.map
                        }
                        KeyCode::Char('6') | KeyCode::Char('v') => {
                            dash.panels.vitals = !dash.panels.vitals
                        }
                        KeyCode::Char('7') | KeyCode::Char('g') => {
                            dash.panels.realms_ranked = !dash.panels.realms_ranked
                        }
                        // Nothing to announce: every color on screen changes,
                        // which is the feedback.
                        KeyCode::Char('t') => {
                            theme::cycle();
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

    fn report(&mut self) -> Snapshot {
        use rand::Rng;
        let mut rng = rand::thread_rng();

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
        };

        // GA returns these ranked; the jitter above would otherwise leave them
        // in their original order with the values out of sequence.
        snapshot.pages.sort_by(|a, b| b.1.total_cmp(&a.1));
        snapshot
    }

    fn live(&mut self) -> (f64, Vec<(String, f64)>) {
        use rand::Rng;
        let mut rng = rand::thread_rng();
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

// ------------------------------------------------------------------- draw ---

fn draw(frame: &mut Frame, dash: &Dash) {
    let area = frame.area();

    // Paint the Osaka Jade ground first: the darkest shade in the palette, so
    // the dashboard looks the same against any terminal background and the
    // panels above it have something to lift off.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::ink()).fg(theme::fg())),
        area,
    );

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
            Constraint::Length(3),
        ])
        .margin(if narrow { 0 } else { 1 })
        .spacing(1)
        .split(area);

    frame.render_widget(header(dash), chunks[0]);
    body(frame, dash, chunks[1], narrow);
    frame.render_widget(footer(dash), chunks[2]);

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
        // Block first, then the realms map, then vitals at the bottom: each
        // takes a box of its own if the column still has the rows for it.
        let mut budget = rect.height;
        let mut extras: Vec<(Stack, u16)> = Vec::new();

        // Block at the top — page block grid, fixed 6 rows.
        if dash.panels.block {
            let needs = 6u16;
            if budget > needs {
                extras.push((Stack::Block, needs));
                budget -= needs + 1;
            }
        }

        // Realms map below the block.
        if dash.panels.map && budget > MAP_ROWS {
            extras.push((Stack::Map, MAP_ROWS));
        }

        // Vitals at the bottom, taking whatever is left.
        let mut constraints: Vec<Constraint> = extras
            .iter()
            .map(|(_, needs)| Constraint::Length(*needs))
            .collect();
        constraints.push(Constraint::Min(VITALS_ROWS));
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .spacing(1)
            .split(rect);

        for ((stack, _), area) in extras.into_iter().zip(rows.iter()) {
            match stack {
                Stack::Map => frame.render_widget(map_panel(dash, area.width, area.height), *area),
                Stack::Block => {
                    frame.render_widget(block_panel(dash, 0, 0, area.width), *area)
                }
            }
        }
        // Vitals is always last — it gets the remaining rows.
        if let Some(last) = rows.last() {
            frame.render_widget(metrics_panel(dash), *last);
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
        (dash.panels.realms_ranked, Column::RealmsRanked, REALMS_RANKED_ROWS),
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
        .position(|(column, _)| {
            matches!(column, Column::Chunks | Column::RealmsRanked)
        })
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
        ("^1 / 1", "block"),
        ("^2 / 2", "right now panel"),
        ("^3 / 3", "top chunks panel"),
        ("^4 / 4", "daily villagers"),
        ("^5 / 5", "realms map"),
        ("^6 / 6", "vitals panel"),
        ("^7 / 7", "top realms"),
        ("t", "next theme"),
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

fn header(dash: &Dash) -> Paragraph<'static> {
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

    // Top realms with their counts — where the players are right now.
    let mut sorted_realms: Vec<&(String, f64)> = dash.live_realms.iter().collect();
    sorted_realms.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (i, (country, count)) in sorted_realms.iter().take(5).enumerate() {
        let abbrev: String = country.chars().take(3).collect();
        if i == 0 {
            spans.push(Span::styled(
                " · ".to_string(),
                Style::default().fg(ore::netherite()),
            ));
        } else {
            spans.push(Span::styled(
                "  ".to_string(),
                Style::default().fg(ore::netherite()),
            ));
        }
        spans.push(Span::styled(
            format!("{abbrev}:{}", *count as u64),
            Style::default()
                .fg(theme::ramp(i))
                .add_modifier(Modifier::BOLD),
        ));
    }

    if dash.in_flight > 0 || dash.live_fetching {
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

    Paragraph::new(lines).block(framed("VITALS", "6", ore::grass()))
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

    let shown = visible_days(inner, dash.daily.len());
    let peak = dash.daily[dash.daily.len() - shown..]
        .iter()
        .cloned()
        .fold(0.0_f64, f64::max);
    let caption = if shown < dash.daily.len() {
        format!(
            "  last {} of {} days · peak {}",
            shown,
            dash.daily.len(),
            commas(peak)
        )
    } else {
        format!("  {shown} days · peak {}", commas(peak))
    };
    lines.push(Line::from(Span::styled(
        caption,
        Style::default().fg(theme::fade(theme::sage(), 0.2)),
    )));

    Paragraph::new(lines).block(framed("DAILY VILLAGERS", "4", ore::grass()))
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
                    Some(color) if color == theme::shadow() => {
                        Span::styled("░", Style::default().fg(color))
                    }
                    Some(color) => Span::styled(
                        "\u{25cf}",
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    None => Span::raw(" "),
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

    Paragraph::new(lines).block(framed("REALMS", "5", ore::lapis()))
}

/// What sits under the vitals in the left-hand column.
enum Stack {
    Map,
    Block,
}

/// Which panel occupies a slot in the right-hand column.
enum Column {
    Live,
    Chunks,
    RealmsRanked,
    Trend,
}

/// The world scene: a row of small blocks, each representing a top page.
/// Block color reflects share of traffic, a gem on top marks rising pages.
fn block_panel(dash: &Dash, _half: usize, _depth: usize, width: u16) -> Paragraph<'static> {
    let w = width.saturating_sub(2) as usize;
    let phase = dash.phase();
    let pages = &dash.pages;

    if pages.is_empty() {
        return Paragraph::new(Line::from(Span::styled(
            "  waiting for data...",
            Style::default().fg(theme::fade(theme::sage(), 0.3)),
        )))
        .block(framed("WORLD", "1", ore::dirt()));
    }

    // How many blocks fit: each block is 4 chars wide + 1 gap.
    let block_w = 4usize;
    let gap = 1usize;
    let count = ((w + gap) / (block_w + gap)).min(pages.len()).min(8);
    let total_w = count * block_w + count.saturating_sub(1) * gap;
    let pad = (w.saturating_sub(total_w)) / 2;

    let mut lines: Vec<Line> = Vec::new();

    // Row 0: gems for rising pages.
    let mut gem_spans: Vec<Span> = Vec::new();
    gem_spans.push(Span::raw(" ".repeat(pad)));
    for (i, row) in pages.iter().take(count).enumerate() {
        if i > 0 {
            gem_spans.push(Span::raw(" ".repeat(gap)));
        }
        let rising = row.moved.map_or(false, |m| m > 0);
        let glyph = if rising { "\u{25c6}" } else { " " };
        let color = if rising {
            ore::emerald()
        } else if row.moved.map_or(false, |m| m < 0) {
            ore::redstone()
        } else {
            theme::shadow()
        };
        gem_spans.push(Span::styled(
            format!("{:^width$}", glyph, width = block_w),
            Style::default().fg(color),
        ));
    }
    lines.push(Line::from(gem_spans));

    // Row 1: block tops (grass cap).
    let mut top_spans: Vec<Span> = Vec::new();
    top_spans.push(Span::raw(" ".repeat(pad)));
    for (i, row) in pages.iter().take(count).enumerate() {
        if i > 0 {
            top_spans.push(Span::raw(" ".repeat(gap)));
        }
        let brightness = row.frac.shown;
        let color = theme::mix(ore::grass(), ore::emerald(), brightness);
        top_spans.push(Span::styled(
            "\u{2588}".repeat(block_w),
            Style::default().fg(color),
        ));
    }
    lines.push(Line::from(top_spans));

    // Row 2: block bodies (dirt).
    let mut body_spans: Vec<Span> = Vec::new();
    body_spans.push(Span::raw(" ".repeat(pad)));
    for (i, row) in pages.iter().take(count).enumerate() {
        if i > 0 {
            body_spans.push(Span::raw(" ".repeat(gap)));
        }
        let brightness = row.frac.shown;
        let color = theme::mix(ore::dirt(), ore::gold(), brightness * 0.4);
        body_spans.push(Span::styled(
            "\u{2588}".repeat(block_w),
            Style::default().fg(color),
        ));
    }
    lines.push(Line::from(body_spans));

    // Row 3: abbreviated page labels.
    let mut label_spans: Vec<Span> = Vec::new();
    label_spans.push(Span::raw(" ".repeat(pad)));
    for (i, row) in pages.iter().take(count).enumerate() {
        if i > 0 {
            label_spans.push(Span::raw(" ".repeat(gap)));
        }
        let short = abbrev(&row.path, block_w);
        label_spans.push(Span::styled(
            format!("{:^width$}", short, width = block_w),
            Style::default().fg(ore::stone()),
        ));
    }
    lines.push(Line::from(label_spans));

    // Row 4: views count under each block.
    let mut view_spans: Vec<Span> = Vec::new();
    view_spans.push(Span::raw(" ".repeat(pad)));
    for (i, row) in pages.iter().take(count).enumerate() {
        if i > 0 {
            view_spans.push(Span::raw(" ".repeat(gap)));
        }
        let val = row.views.shown;
        let label = if val >= 1000.0 {
            format!("{:.0}k", val / 1000.0)
        } else {
            format!("{:.0}", val)
        };
        view_spans.push(Span::styled(
            format!("{:^width$}", label, width = block_w),
            Style::default().fg(theme::fade(ore::stone(), 0.2)),
        ));
    }
    lines.push(Line::from(view_spans));

    // Bottom: animated torch flicker.
    let torch_x = ((phase * 3.0) as usize % (w / 2)) + w / 4;
    let mut torch_spans: Vec<Span> = Vec::new();
    torch_spans.push(Span::raw(" ".repeat(torch_x)));
    let flicker = (phase * 8.0).sin() * 0.5 + 0.5;
    torch_spans.push(Span::styled(
        "\u{25cf}",
        Style::default().fg(theme::mix(ore::gold(), ore::redstone(), flicker)),
    ));
    lines.push(Line::from(torch_spans));

    Paragraph::new(lines).block(framed("WORLD", "1", ore::dirt()))
}

/// Shorten a page path to fit `max` characters.
fn abbrev(path: &str, max: usize) -> String {
    let clean = path.trim_start_matches('/');
    if clean.len() <= max {
        return clean.to_string();
    }
    if max <= 1 {
        return clean.chars().take(1).collect();
    }
    // Take first (max-1) chars and append "~".
    let mut s: String = clean.chars().take(max - 1).collect();
    s.push('~');
    s
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
    if dash.feed.is_empty() {
        lines.push(Line::from(Span::styled(
            "  quiet out there",
            Style::default().fg(theme::fade(theme::sage(), 0.3)),
        )));
    } else {
        for entry in dash.feed.iter().take(6) {
            let age = entry.at.elapsed().as_secs_f64() / FEED_TTL.as_secs_f64();
            let rising = entry.delta > 0.0;
            let base = if rising {
                theme::bright()
            } else {
                ore::ender()
            };
            let time_ago = entry.at.elapsed().as_secs();
            let time_str = if time_ago < 60 {
                format!("{}s", time_ago)
            } else {
                format!("{}m", time_ago / 60)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", if rising { glyph::UP } else { glyph::DOWN }),
                    Style::default().fg(theme::fade(base, age)),
                ),
                Span::styled(
                    format!("{:>4} ", format!("{:+}", entry.delta as i64)),
                    Style::default()
                        .fg(theme::fade(base, age))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if rising { "spawned in" } else { "wandered off" }.to_string(),
                    Style::default().fg(theme::fade(theme::sage(), age)),
                ),
                Span::styled(
                    format!("  {}", time_str),
                    Style::default().fg(theme::fade(theme::shadow(), age)),
                ),
            ]));
        }
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

    Paragraph::new(lines).block(framed("TOP CHUNKS", "3", ore::copper()))
}

fn realms_ranked_panel(dash: &Dash, width: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    let inner = width.saturating_sub(2) as usize;
    let cells = inner.saturating_sub(3 + 2 + VIEWS_COLUMN).clamp(4, 20);
    let label_cells = inner.saturating_sub(4 + MOVED_COLUMN);

    let peak = dash
        .realms
        .iter()
        .map(|(_, v)| *v)
        .fold(0.0_f64, f64::max);

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
                Style::default()
                    .fg(ore_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{label:<label_cells$}"),
                Style::default().fg(color),
            ),
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

    Paragraph::new(lines).block(framed("TOP REALMS", "7", ore::lapis()))
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
