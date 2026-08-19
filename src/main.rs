//! anacraft — Google Analytics 4 in your terminal, wearing a texture pack.

mod achievements;
mod auth;
mod config;
mod ga;
mod render;
mod theme;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use config::Config;
use ga::{DateRange, Ga, ReportRequest};
use render::{bold, dim, paint, panel_bottom, panel_top};
use theme::{ore, OVERVIEW};

#[derive(Parser)]
#[command(
    name = "anacraft",
    version,
    about = "Google Analytics, mined block by block",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// GA4 property id to query, overriding the saved default.
    #[arg(long, short, global = true)]
    property: Option<String>,

    /// Palette to render with, overriding the saved default.
    #[arg(long, short, global = true)]
    theme: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with Google and store a refresh token.
    Login,
    /// Revoke and delete the stored credentials.
    Logout,
    /// List every GA4 property this account can read.
    Props,
    /// Set the default property.
    Use {
        /// Numeric property id, e.g. 397412345
        id: String,
    },
    /// Headline metrics for the period, with deltas and achievements.
    Overview {
        /// Days to look back, ending yesterday.
        #[arg(long, short, default_value_t = 7)]
        days: u32,
    },
    /// Most-visited pages.
    Pages {
        #[arg(long, short, default_value_t = 7)]
        days: u32,
        #[arg(long, short, default_value_t = 10)]
        limit: i64,
    },
    /// Where traffic arrives from.
    Portals {
        #[arg(long, short, default_value_t = 7)]
        days: u32,
        #[arg(long, short, default_value_t = 10)]
        limit: i64,
    },
    /// Traffic by country.
    Realms {
        #[arg(long, short, default_value_t = 7)]
        days: u32,
        #[arg(long, short, default_value_t = 10)]
        limit: i64,
    },
    /// Who is on the site right now.
    Live,
    /// Render a dashboard from synthetic data — no Google account needed.
    Demo,
    /// List the palettes, or save one as the default.
    Theme {
        /// Palette name. Omit to list what is available.
        name: Option<String>,
    },
    /// Full-screen live dashboard.
    Dash {
        #[arg(long, short, default_value_t = 7)]
        days: u32,
        /// Seconds between refreshes.
        #[arg(long, default_value_t = 30)]
        refresh: u64,
        /// Seconds between realtime polls — the htop-style tick. Minimum 2.
        #[arg(long, default_value_t = ui::LIVE_EVERY)]
        live_refresh: u64,
        /// Drive the dashboard from synthetic data — no Google account needed.
        #[arg(long)]
        demo: bool,
    },
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!(
            "\n  {} {}\n",
            paint("⛏ ", ore::redstone()),
            bold(&err.to_string())
        );
        // Surface the cause chain, which is where API detail usually lives.
        for cause in err.chain().skip(1) {
            eprintln!("     {}", dim(&cause.to_string()));
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;

    // Resolve the palette before anything renders: the flag wins, then the
    // saved default. An unknown name is worth saying out loud rather than
    // silently falling back.
    if let Some(name) = cli.theme.as_deref().or(cfg.theme.as_deref()) {
        if !theme::select(name) {
            anyhow::bail!("no theme called {name} — run `anacraft theme` to list them");
        }
    }

    match cli.command.unwrap_or(Command::Dash {
        days: 7,
        refresh: 30,
        live_refresh: ui::LIVE_EVERY,
        demo: cfg.property_id.is_none(),
    }) {
        Command::Demo => cmd_demo(),
        Command::Theme { name } => cmd_theme(name.as_deref()),
        Command::Login => cmd_login().await,
        Command::Logout => cmd_logout().await,
        Command::Props => cmd_props().await,
        Command::Use { id } => cmd_use(&id).await,
        Command::Overview { days } => {
            cmd_overview(&cfg.resolve_property(cli.property.as_deref())?, days).await
        }
        Command::Pages { days, limit } => {
            let property = cfg.resolve_property(cli.property.as_deref())?;
            cmd_ranked(
                &property,
                days,
                limit,
                "pagePath",
                "screenPageViews",
                "views",
                "TOP CHUNKS",
            )
            .await
        }
        Command::Portals { days, limit } => {
            let property = cfg.resolve_property(cli.property.as_deref())?;
            cmd_ranked(
                &property,
                days,
                limit,
                "sessionSourceMedium",
                "sessions",
                "expeditions",
                "PORTALS",
            )
            .await
        }
        Command::Realms { days, limit } => {
            let property = cfg.resolve_property(cli.property.as_deref())?;
            cmd_ranked(
                &property,
                days,
                limit,
                "country",
                "totalUsers",
                "villagers",
                "REALMS",
            )
            .await
        }
        Command::Live => cmd_live(&cfg.resolve_property(cli.property.as_deref())?).await,
        Command::Dash {
            days,
            refresh,
            live_refresh,
            demo,
        } => {
            if demo {
                // Checked before resolving a property, so the demo works on a
                // machine that has never logged in. The report cadence is
                // pinned short so the synthetic numbers visibly move.
                return ui::run_demo(days, refresh.min(5), live_refresh).await;
            }
            let property = cfg.resolve_property(cli.property.as_deref())?;
            ui::run(&property, days, refresh, live_refresh).await
        }
    }
}

// ---------------------------------------------------------------- accounts ---

async fn cmd_login() -> Result<()> {
    let http = reqwest::Client::new();
    let auth = auth::Auth::new(http)?;
    auth.login().await?;

    println!("  {} logged in.\n", paint("✓", ore::emerald()));

    // A fresh login with no property selected is a dead end; nudge onward.
    let cfg = Config::load()?;
    if cfg.property_id.is_none() {
        println!("  next: {} to pick a property\n", bold("anacraft props"));
    }
    Ok(())
}

async fn cmd_logout() -> Result<()> {
    let http = reqwest::Client::new();
    auth::Auth::new(http)?.logout().await?;
    println!("  {} signed out.\n", paint("✓", ore::emerald()));
    Ok(())
}

async fn cmd_props() -> Result<()> {
    let client = Ga::new()?;
    let props = client.properties().await?;

    if props.is_empty() {
        println!("\n  no GA4 properties visible to this account.\n");
        return Ok(());
    }

    let cfg = Config::load()?;
    println!("\n{}\n", panel_top("PROPERTIES"));
    for (i, prop) in props.iter().enumerate() {
        let current = cfg.property_id.as_deref() == Some(prop.id.as_str());
        let marker = if current {
            paint("●", ore::emerald())
        } else {
            dim("○")
        };
        println!(
            "  {marker} {}  {}\n      {}",
            bold(&paint(&prop.name, theme::ramp(i))),
            dim(&prop.account),
            dim(&format!("id {}", prop.id)),
        );
    }
    println!("\n  set one with {}\n", bold("anacraft use <id>"));
    println!("{}\n", panel_bottom());
    Ok(())
}

async fn cmd_use(id: &str) -> Result<()> {
    let client = Ga::new()?;
    let props = client.properties().await?;
    let wanted = id.trim().trim_start_matches("properties/");

    let found = props
        .iter()
        .find(|p| p.id == wanted)
        .with_context(|| format!("no property {wanted} on this account — run `anacraft props`"))?;

    let mut cfg = Config::load()?;
    cfg.property_id = Some(found.id.clone());
    cfg.property_name = Some(found.name.clone());
    cfg.save()?;

    println!(
        "\n  {} mining {} {}\n",
        paint("✓", ore::emerald()),
        bold(&paint(&found.name, ore::diamond())),
        dim(&format!("({})", found.id)),
    );
    Ok(())
}

/// With a name, saves it as the default. Without, lists what there is.
fn cmd_theme(name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        if !theme::select(name) {
            anyhow::bail!("no theme called {name} — run `anacraft theme` to list them");
        }
        let mut cfg = Config::load()?;
        cfg.theme = Some(name.to_string());
        cfg.save()?;
        println!(
            "\n  {} theme set to {}\n",
            paint("✓", ore::emerald()),
            bold(&paint(name, ore::diamond())),
        );
        return Ok(());
    }

    let current = theme::palette().name;
    println!("\n{}\n", panel_top("THEMES"));
    for palette in theme::THEMES {
        let selected = palette.name == current;
        let marker = if selected {
            paint("●", ore::emerald())
        } else {
            dim("○")
        };
        // A swatch beats a name: each row is drawn in the palette it names,
        // not in whichever one happens to be selected.
        let swatch: String = (0..8)
            .map(|i| paint("███", theme::ramp_of(palette, i)))
            .collect::<Vec<_>>()
            .join("");
        println!("  {marker} {:<14} {swatch}", bold(palette.name));
    }
    println!("\n  set one with {}\n", bold("anacraft theme <name>"));
    println!("{}\n", panel_bottom());
    Ok(())
}

// ---------------------------------------------------------------- reports ---

async fn cmd_overview(property: &str, days: u32) -> Result<()> {
    let client = Ga::new()?;
    let metrics: Vec<&str> = OVERVIEW.iter().map(|m| m.api).collect();

    let current = client
        .report(
            property,
            ReportRequest::new(&metrics).range(DateRange::last_days(days)),
        )
        .await?;

    let previous = client
        .report(
            property,
            ReportRequest::new(&metrics).range(DateRange::previous_days(days)),
        )
        .await?;

    let trend = client
        .report(
            property,
            ReportRequest::new(&["totalUsers"])
                .by(&["date"])
                .range(DateRange::last_days(days)),
        )
        .await?;

    // GA returns date rows unordered; the sparkline needs them chronological.
    let mut daily = trend.rows.clone();
    daily.sort_by(|a, b| a.dimension(0).cmp(b.dimension(0)));
    let series: Vec<f64> = daily.iter().map(|r| r.metric(0)).collect();

    let empty = current.rows.is_empty() && current.totals.is_empty();
    let cfg = Config::load()?;
    let title = cfg
        .property_name
        .clone()
        .unwrap_or_else(|| format!("property {property}"));

    let totals: Vec<f64> = (0..OVERVIEW.len()).map(|i| current.total(i)).collect();
    let prior: Vec<f64> = (0..OVERVIEW.len()).map(|i| previous.total(i)).collect();

    print_overview(&title, days, &totals, &prior, &series, empty);
    Ok(())
}

/// Shared by `overview` and `demo` so both render through one code path.
fn print_overview(
    title: &str,
    days: u32,
    current: &[f64],
    previous: &[f64],
    series: &[f64],
    empty: bool,
) {
    println!(
        "\n{}\n",
        panel_top(&format!("{} · last {days} days", title.to_uppercase()))
    );

    if empty {
        println!("  {}\n", dim("no data in this window."));
        println!("{}\n", panel_bottom());
        return;
    }

    for (i, metric) in OVERVIEW.iter().enumerate() {
        let now = current.get(i).copied().unwrap_or(0.0);
        let before = previous.get(i).copied().unwrap_or(0.0);
        print!("{}", render::metric_block(metric, now, before));
    }

    if series.len() > 1 {
        println!(
            "  {}  {}\n",
            dim("daily villagers"),
            render::sparkline(series, ore::grass())
        );
    }

    let snap = achievements::Snapshot {
        users: current[0],
        prev_users: previous[0],
        sessions: current[1],
        views: current[2],
        conversions: current[3],
        prev_conversions: previous[3],
        bounce_rate: current[4],
        prev_bounce_rate: previous[4],
        avg_duration: current[5],
        daily_users: series.to_vec(),
    };

    for achievement in achievements::unlocked(&snap).iter().take(3) {
        print!("{}", render::toast(achievement.title, &achievement.detail));
    }

    println!("\n{}\n", panel_bottom());
}

/// Synthetic numbers shaped like a small site having a good week, so the theme
/// can be evaluated (and screenshotted) before anyone connects an account.
fn cmd_demo() -> Result<()> {
    let series = vec![1402.0, 1288.0, 1531.0, 1495.0, 1760.0, 1834.0, 1971.0];
    let current = vec![12_481.0, 18_203.0, 41_776.0, 312.0, 0.412, 214.0];
    let previous = vec![11_450.0, 17_004.0, 39_210.0, 258.0, 0.478, 191.0];

    print_overview(
        "Redstone Labs (demo)",
        7,
        &current,
        &previous,
        &series,
        false,
    );

    println!(
        "  {}\n",
        dim("synthetic data — run `anacraft login` to connect a real property")
    );
    Ok(())
}

/// Shared implementation for pages / portals / realms — same shape, different
/// dimension and metric.
async fn cmd_ranked(
    property: &str,
    days: u32,
    limit: i64,
    dimension: &str,
    metric: &str,
    unit: &str,
    title: &str,
) -> Result<()> {
    let client = Ga::new()?;
    let report = client
        .report(
            property,
            ReportRequest::new(&[metric])
                .by(&[dimension])
                .range(DateRange::last_days(days))
                .top(metric, limit),
        )
        .await?;

    println!("\n{}\n", panel_top(&format!("{title} · last {days} days")));

    if report.is_empty() {
        println!("  {}\n", dim("no data in this window."));
        println!("{}\n", panel_bottom());
        return Ok(());
    }

    println!(
        "  {}\n",
        dim(&format!(
            "{} ranked by {}",
            theme::dimension_label(dimension),
            unit
        ))
    );

    let rows: Vec<(String, f64)> = report
        .rows
        .iter()
        .map(|r| (r.dimension(0).to_string(), r.metric(0)))
        .collect();

    print!("{}", render::ranked_table(&rows, unit, 26));
    println!("\n{}\n", panel_bottom());
    Ok(())
}

async fn cmd_live(property: &str) -> Result<()> {
    let client = Ga::new()?;
    let report = client
        .realtime(
            property,
            ReportRequest::new(&["activeUsers"])
                .by(&["country"])
                .top("activeUsers", 10),
        )
        .await?;

    let total: f64 = report.rows.iter().map(|r| r.metric(0)).sum();

    println!("\n{}\n", panel_top("RIGHT NOW"));
    println!(
        "  {} {}\n",
        bold(&paint(&render::commas(total), ore::xp())),
        dim("players online (last 30 min)"),
    );

    if !report.is_empty() {
        let rows: Vec<(String, f64)> = report
            .rows
            .iter()
            .map(|r| (r.dimension(0).to_string(), r.metric(0)))
            .collect();
        print!("{}", render::ranked_table(&rows, "online", 26));
    }

    println!("\n{}\n", panel_bottom());
    Ok(())
}
