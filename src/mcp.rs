//! `craft mcp` — the dashboard's numbers, over the Model Context Protocol.
//!
//! Stdio only. An MCP client spawns the server as a child process and talks
//! newline-delimited JSON-RPC over its pipes, so there is no port to open and
//! no listener to secure. The one hard rule that follows: **stdout belongs to
//! the protocol**. Everything human-facing goes to stderr, which is why the
//! error printing in `main.rs` uses `eprintln!`.
//!
//! Every tool is a read. Nothing here writes to `~/.anacraft/`, starts an OAuth
//! flow, or moves the default property — `login` and `use` stay human-only
//! commands. An agent should not be able to silently repoint the tool at
//! another property, and a browser consent flow has no business running inside
//! a client's subprocess.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::achievements;
use crate::config::Config;
use crate::ga::{DateRange, Ga, ReportRequest};
use crate::theme::{Kind, OVERVIEW};

/// The revision we answer `initialize` with when the client asks for one we
/// don't know. Older revisions are echoed back when the client names them: the
/// tool surface is identical across all four, so there is nothing to gain by
/// telling a working client to speak a dialect it doesn't have.
const PROTOCOL_VERSION: &str = "2025-11-25";
const SPOKEN_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The revision that introduced `icons` on the server's own identity. A client
/// speaking an older one is not told about the mark: an unknown key is likely
/// to be ignored, but there is no reason to make a handshake carry half a
/// kilobyte that the other end has no field for.
const ICONS_SINCE: &str = "2025-11-25";

/// The mark, drawn by `scripts/gen-logo.py` from the same 16x16 grid as the
/// favicon and the OAuth logo. It travels inside the handshake as a data URI
/// rather than as a link to the site: a client that draws it should not have
/// to make a network request — or tell anyone it did — to know what anacraft
/// looks like.
const ICON_PNG: &[u8] = include_bytes!("../assets/icon-128.png");

const DEFAULT_DAYS: u32 = 7;
const DEFAULT_LIMIT: i64 = 10;

/// Identical reports are served from memory for this long. Quota is shared with
/// `craft dash`, and an agent in a loop can out-ask a human by orders of
/// magnitude — a minute is short enough that nobody reads a stale number and
/// long enough that a chatty client costs one request instead of forty.
const REPORT_TTL: Duration = Duration::from_secs(60);
/// "Who is on the site right now" is a different question, and a minute-old
/// answer to it is the wrong answer.
const LIVE_TTL: Duration = Duration::from_secs(10);

/// What the assistant is told the server is for, once, at handshake time.
const INSTRUCTIONS: &str = "\
Read-only Google Analytics 4 for the site this machine is signed in to. \
Every tool takes an optional `property` (a numeric GA4 property id) and falls \
back to the saved default, so you can call them without knowing the config. \
Numbers are labelled with their unit and carry the date window they cover — \
quote the window when you quote a number. `site_status` answers \"how is the \
site doing\" in one call; the `list_*` tools rank a dimension; the `search_*` \
tools filter one by a substring.";

// ------------------------------------------------------------------ entry ---

/// Serve until the client closes stdin.
pub async fn serve(demo: bool, property: Option<&str>) -> Result<()> {
    use crate::render::paint;
    use crate::theme::ore;

    let cfg = Config::load()?;

    // Same check the dashboard runs on the way in: ask Supabase where the
    // subscription stands, cache the answer, and write it back to the config.
    // Cheap, short-timeout and best-effort — see `license::sync`.
    let supporter = if demo {
        false
    } else {
        crate::license::sync(cfg.supporter).await
    };

    let source = if demo {
        // The demo is the shop window: no account, no subscription, no gate.
        // It exists so the server can be wired into a client and looked at
        // before anyone signs in or pays.
        Source::Demo
    } else {
        // Nothing here exits the process. A client spawns this server as a
        // subprocess, and an early exit reaches the user as "server
        // disconnected" — a sentence about pipes that says nothing about the
        // subscription or the missing login that actually caused it. So an
        // unmet requirement locks the tools instead: the handshake succeeds,
        // the client stays connected, and every call answers with the one
        // sentence that gets the user unstuck.
        match unlock(supporter) {
            Ok(ga) => Source::Api(Box::new(ga)),
            Err(reason) => {
                // stderr is the client's log, and the protocol owns stdout.
                eprintln!("\n  {} {reason}\n", paint("⛏", ore::redstone()));
                Source::Locked(reason)
            }
        }
    };

    Server {
        cfg,
        source,
        property: property.map(str::to_string),
        cache: HashMap::new(),
    }
    .run()
    .await
}

/// Everything the live tools need, or the one sentence explaining what is
/// missing. The `Err` is a message for a human and for the assistant relaying
/// it, never a reason to stop serving — see `serve`.
fn unlock(supporter: bool) -> std::result::Result<Ga, String> {
    subscription(supporter)?;
    login()?;
    // A client is not a place to open a browser, so this only builds the HTTP
    // client and reads the stored credentials; consent stays in `craft login`.
    Ga::new().map_err(|err| format!("could not start the GA4 client: {err}"))
}

/// The subscriber gate.
///
/// Takes the answer rather than reading the config, because by the time this
/// runs `serve` has already asked Supabase and written what came back — see
/// `license::sync`. It is still a soft gate: the flag it consults is a line of
/// TOML in a config anybody can edit, in a binary anybody can rebuild.
fn subscription(supporter: bool) -> std::result::Result<(), String> {
    if supporter {
        return Ok(());
    }
    Err(format!(
        "craft mcp is part of the Anacraft subscription.\n     \
         Run `craft subscribe` to start one — it writes `supporter = true` in {} \
         once the payment clears. Already subscribed on another machine? \
         `craft login` with the same Google account, then `craft subscribe --check`.\n     \
         `craft mcp --demo` serves synthetic data and needs no subscription.",
        Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "your config".into())
    ))
}

/// Said once at startup rather than discovered on the first tool call.
fn login() -> std::result::Result<(), String> {
    match crate::auth::Tokens::load() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(
            "not logged in — run `craft login` in a terminal, then restart the MCP client".into(),
        ),
        Err(err) => Err(format!("could not read the stored credentials: {err}")),
    }
}

enum Source {
    /// Boxed: `Ga` owns an HTTP client, and an enum sized by its largest
    /// variant would make every `Source::Demo` carry that weight.
    Api(Box<Ga>),
    Demo,
    /// Serving, but with nothing to serve: the subscription or the login is
    /// missing, and this is the sentence to hand back instead of numbers.
    Locked(String),
}

struct Server {
    cfg: Config,
    source: Source,
    /// `--property` from the command line, used when a tool call names none.
    property: Option<String>,
    cache: HashMap<String, (Instant, Value)>,
}

impl Server {
    async fn run(&mut self) -> Result<()> {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let mut out = tokio::io::stdout();

        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let reply = match serde_json::from_str::<Value>(line) {
                Ok(message) => self.dispatch(message).await,
                Err(err) => Some(rpc_error(
                    Value::Null,
                    -32700,
                    &format!("invalid JSON: {err}"),
                )),
            };

            if let Some(reply) = reply {
                let mut body = serde_json::to_string(&reply)?;
                body.push('\n');
                out.write_all(body.as_bytes()).await?;
                out.flush().await?;
            }
        }
        Ok(())
    }

    /// One message in, at most one message out. `None` means "say nothing",
    /// which is the correct answer to a notification and to anything that
    /// isn't a request at all.
    async fn dispatch(&mut self, message: Value) -> Option<Value> {
        let method = message.get("method")?.as_str()?.to_string();
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

        let outcome = match method.as_str() {
            "initialize" => Ok(initialize(&params, &self.source)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_schemas() })),
            "tools/call" => self.call(&params).await,
            _ => {
                // Notifications we don't act on are still well-formed traffic;
                // only a *request* for an unknown method deserves an error.
                let id = id?;
                return Some(rpc_error(id, -32601, &format!("unknown method {method}")));
            }
        };

        let id = id?;
        Some(match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(err) => rpc_error(id, -32602, &err.to_string()),
        })
    }

    async fn call(&mut self, params: &Value) -> Result<Value> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .context("tools/call needs a tool name")?
            .to_string();
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        if !TOOLS.iter().any(|tool| tool.name == name) {
            bail!("no tool called {name}");
        }

        match self.tool(&name, &args).await {
            Ok(value) => Ok(tool_result(value, false)),
            // A rejected property id or an expired login is something the
            // assistant can reason about and relay; a JSON-RPC error would
            // instead read to most clients as the server being broken.
            Err(err) => Ok(tool_result(json!({ "error": err.to_string() }), true)),
        }
    }

    /// A tool call, served from the cache when the same question was asked a
    /// moment ago.
    async fn tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        let ttl = if name == "live_visitors" {
            LIVE_TTL
        } else {
            REPORT_TTL
        };
        let key = format!("{name}|{}|{args}", self.property.as_deref().unwrap_or(""));

        if let Some(hit) = self.cached(&key, ttl) {
            return Ok(hit);
        }
        let fresh = self.fetch(name, args).await?;
        self.cache.insert(key, (Instant::now(), fresh.clone()));
        Ok(fresh)
    }

    fn cached(&mut self, key: &str, ttl: Duration) -> Option<Value> {
        // Every miss is a chance to drop what has gone stale, which keeps a
        // long-lived server from accumulating every window ever asked for.
        self.cache.retain(|_, (at, _)| at.elapsed() < REPORT_TTL);

        let (at, value) = self.cache.get(key)?;
        if at.elapsed() >= ttl {
            return None;
        }
        let mut value = value.clone();
        if let Some(object) = value.as_object_mut() {
            // Said out loud, so an assistant watching a number that won't move
            // knows why rather than concluding the site went quiet.
            object.insert("cached".into(), json!(true));
        }
        Some(value)
    }

    async fn fetch(&self, name: &str, args: &Value) -> Result<Value> {
        let days = days_of(args);
        let limit = limit_of(args);

        let ga = match &self.source {
            Source::Demo => return demo::tool(name, args, days, limit),
            Source::Locked(reason) => bail!("{}", one_line(reason)),
            Source::Api(ga) => ga.as_ref(),
        };

        if name == "list_properties" {
            let props = ga.properties().await?;
            return Ok(json!({
                "properties": props.iter().map(|p| json!({
                    "property": p.id,
                    "name": p.name,
                    "account": p.account,
                    "is_default": self.cfg.active.as_deref() == Some(p.id.as_str()),
                })).collect::<Vec<_>>(),
            }));
        }

        let property = self.resolve(args)?;
        let named = self.cfg.find(&property).map(|p| p.display());

        let payload = match name {
            "site_status" => site_status(ga, &property, days).await?,
            "live_visitors" => live_visitors(ga, &property).await?,
            "list_pages" => {
                ranked(
                    ga,
                    &property,
                    days,
                    limit,
                    "pagePath",
                    "screenPageViews",
                    None,
                )
                .await?
            }
            "list_events" => list_events(ga, &property, days, limit).await?,
            "list_referrers" => {
                ranked(ga, &property, days, limit, "pageReferrer", "sessions", None).await?
            }
            "list_traffic_sources" => {
                ranked(
                    ga,
                    &property,
                    days,
                    limit,
                    "sessionSourceMedium",
                    "sessions",
                    None,
                )
                .await?
            }
            "list_countries" => {
                ranked(ga, &property, days, limit, "country", "totalUsers", None).await?
            }
            "search_pages" => {
                let query = query_of(args)?;
                ranked(
                    ga,
                    &property,
                    days,
                    limit,
                    "pagePath",
                    "screenPageViews",
                    Some(&query),
                )
                .await?
            }
            "search_events" => {
                let query = query_of(args)?;
                ranked(
                    ga,
                    &property,
                    days,
                    limit,
                    "eventName",
                    "eventCount",
                    Some(&query),
                )
                .await?
            }
            other => bail!("no tool called {other}"),
        };

        let window = if name == "live_visitors" {
            None
        } else {
            Some(days)
        };
        Ok(envelope(&property, named.as_deref(), window, payload))
    }

    /// The property a call is about: its own argument, then `--property`, then
    /// whatever is saved. Nothing here writes the choice back.
    fn resolve(&self, args: &Value) -> Result<String> {
        let asked = args
            .get("property")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.property.clone());
        self.cfg.resolve_property(asked.as_deref())
    }
}

// ------------------------------------------------------------------ shapes ---

fn initialize(params: &Value, source: &Source) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let version = if SPOKEN_VERSIONS.contains(&asked) {
        asked
    } else {
        PROTOCOL_VERSION
    };

    // The tools are still listed when the server is locked — an assistant that
    // knows what it cannot reach, and why, can say so; one that sees no tools
    // at all can only guess the wiring is broken.
    let instructions = match source {
        Source::Api(_) => INSTRUCTIONS.to_string(),
        // Said at the handshake as well as on every payload: an assistant that
        // learns the numbers were invented only after quoting them has already
        // misled someone.
        Source::Demo => format!(
            "{INSTRUCTIONS}\n\nThis server is running on synthetic demo data. \
             Every number is invented, every answer carries `synthetic: true`, \
             and none of it describes a real site — say so whenever you quote one."
        ),
        Source::Locked(reason) => format!(
            "{INSTRUCTIONS}\n\nThe tools cannot reach GA4 right now, and every \
             call will return this until it is fixed. Tell the user, verbatim: \
             {}",
            one_line(reason)
        ),
    };

    let mut server_info = json!({
        "name": "anacraft",
        "title": "Anacraft",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Google Analytics 4 for the terminal, read-only over MCP.",
        "websiteUrl": "https://anacraft.dev",
    });
    // Dates sort as strings, so this stays right when the spoken revision moves on.
    if version >= ICONS_SINCE {
        server_info["icons"] = json!([icon()]);
    }

    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": server_info,
        "instructions": instructions,
    })
}

/// The mark as an `Icon`, for a client that draws one next to the server's
/// name. Encoded once and kept: the bytes never change within a run, and a
/// handshake is not the place to redo work.
fn icon() -> &'static Value {
    static ICON: OnceLock<Value> = OnceLock::new();
    ICON.get_or_init(|| {
        let src = format!("data:image/png;base64,{}", STANDARD.encode(ICON_PNG));
        json!({ "src": src, "mimeType": "image/png", "sizes": ["128x128"] })
    })
}

/// A locked reason is written to be read in a terminal, where the indent
/// carries a wrapped line. JSON has no such column, so flatten it before it
/// travels as a string.
fn one_line(reason: &str) -> String {
    reason.replace("\n     ", " ")
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// A tool result carries the same JSON twice: as text, which every client can
/// render, and as `structuredContent`, which the newer ones parse. Ore-textured
/// bars are for humans; a model wants labelled numbers.
fn tool_result(value: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": is_error,
    })
}

/// Every answer says which property it is about and which days it covers, so an
/// assistant can attribute the numbers it quotes.
fn envelope(property: &str, name: Option<&str>, days: Option<u32>, payload: Value) -> Value {
    let mut out = json!({ "property": property });
    let object = out.as_object_mut().expect("built as an object");

    if let Some(name) = name {
        object.insert("property_name".into(), json!(name));
    }
    match days {
        Some(days) => {
            let range = DateRange::last_days(days);
            object.insert(
                "date_range".into(),
                json!({
                    "start_date": range.start_date,
                    "end_date": range.end_date,
                    "days": days,
                    "note": "GA4 relative dates; the window ends yesterday, because today is still partial",
                }),
            );
        }
        None => {
            object.insert("window".into(), json!("the last 30 minutes"));
        }
    }
    if let Some(payload) = payload.as_object() {
        for (key, value) in payload {
            object.insert(key.clone(), value.clone());
        }
    }
    out
}

// ------------------------------------------------------------------- tools ---

struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    /// Which optional arguments the tool takes, beyond `property`.
    days: bool,
    limit: bool,
    query: Option<&'static str>,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "site_status",
        title: "Site status",
        description: "How the site is doing: users, sessions, page views, key events, bounce rate \
                      and average session duration for the period, each against the equivalent \
                      period before it, plus the daily user series and any milestones the numbers \
                      crossed. Start here for \"how are we doing\".",
        days: true,
        limit: false,
        query: None,
    },
    Tool {
        name: "live_visitors",
        title: "Live visitors",
        description: "Who is on the site right now — active users in the last 30 minutes, broken \
                      down by country. Realtime, so it ignores the `days` window entirely.",
        days: false,
        limit: false,
        query: None,
    },
    Tool {
        name: "list_pages",
        title: "Top pages",
        description: "Most-visited pages over the period, ranked by page views.",
        days: true,
        limit: true,
        query: None,
    },
    Tool {
        name: "list_events",
        title: "Top events",
        description: "Events over the period, ranked by count, with the per-day total for this \
                      period and the one before it so a rise or fall is visible.",
        days: true,
        limit: true,
        query: None,
    },
    Tool {
        name: "list_referrers",
        title: "Top referrers",
        description: "The URLs that sent traffic, ranked by sessions. Use this for \"who is \
                      linking to us\"; use list_traffic_sources for the channel breakdown.",
        days: true,
        limit: true,
        query: None,
    },
    Tool {
        name: "list_traffic_sources",
        title: "Traffic sources",
        description: "Where traffic arrives from, as GA4's source / medium pairs \
                      (google / organic, (direct) / (none), …), ranked by sessions.",
        days: true,
        limit: true,
        query: None,
    },
    Tool {
        name: "list_countries",
        title: "Traffic by country",
        description: "Countries the period's users came from, ranked by users.",
        days: true,
        limit: true,
        query: None,
    },
    Tool {
        name: "list_properties",
        title: "GA4 properties",
        description: "Every GA4 property the signed-in account can read, and which one is the \
                      saved default. Read-only: this cannot change the default.",
        days: false,
        limit: false,
        query: None,
    },
    Tool {
        name: "search_pages",
        title: "Search pages",
        description: "Pages whose path contains a substring, ranked by page views — \
                      \"/blog\", \"pricing\", a slug you half remember.",
        days: true,
        limit: true,
        query: Some("Substring to match against the page path, case-insensitive."),
    },
    Tool {
        name: "search_events",
        title: "Search events",
        description: "Events whose name contains a substring, ranked by count — \
                      \"signup\", \"purchase\", \"click\".",
        days: true,
        limit: true,
        query: Some("Substring to match against the event name, case-insensitive."),
    },
];

fn tool_schemas() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|tool| {
            let mut properties = serde_json::Map::new();
            let mut required: Vec<&str> = Vec::new();

            if let Some(help) = tool.query {
                properties.insert(
                    "query".into(),
                    json!({ "type": "string", "description": help }),
                );
                required.push("query");
            }
            if tool.days {
                properties.insert(
                    "days".into(),
                    json!({
                        "type": "integer",
                        "description": "Days to look back, ending yesterday.",
                        "default": DEFAULT_DAYS,
                        "minimum": 1,
                        "maximum": 365,
                    }),
                );
            }
            if tool.limit {
                properties.insert(
                    "limit".into(),
                    json!({
                        "type": "integer",
                        "description": "How many rows to return.",
                        "default": DEFAULT_LIMIT,
                        "minimum": 1,
                        "maximum": 100,
                    }),
                );
            }
            if tool.name != "list_properties" {
                properties.insert(
                    "property".into(),
                    json!({
                        "type": "string",
                        "description": "Numeric GA4 property id. Omit to use the saved default.",
                    }),
                );
            }

            json!({
                "name": tool.name,
                "title": tool.title,
                "description": tool.description,
                "inputSchema": {
                    "type": "object",
                    "properties": properties,
                    "required": required,
                },
                "annotations": { "readOnlyHint": true, "openWorldHint": true },
            })
        })
        .collect()
}

// ------------------------------------------------------------------ reports ---

async fn site_status(ga: &Ga, property: &str, days: u32) -> Result<Value> {
    let metrics: Vec<&str> = OVERVIEW.iter().map(|m| m.api).collect();

    let (current, previous, trend) = tokio::try_join!(
        ga.report(
            property,
            ReportRequest::new(&metrics).range(DateRange::last_days(days))
        ),
        ga.report(
            property,
            ReportRequest::new(&metrics).range(DateRange::previous_days(days))
        ),
        ga.report(
            property,
            ReportRequest::new(&["totalUsers"])
                .by(&["date"])
                .range(DateRange::last_days(days))
        )
    )?;

    let totals: Vec<f64> = (0..OVERVIEW.len()).map(|i| current.total(i)).collect();
    let prior: Vec<f64> = (0..OVERVIEW.len()).map(|i| previous.total(i)).collect();

    // GA returns date rows unordered, and a series read out of order is a
    // different story than the one the numbers tell.
    let mut rows = trend.rows.clone();
    rows.sort_by(|a, b| a.dimension(0).cmp(b.dimension(0)));
    let daily: Vec<(String, f64)> = rows
        .iter()
        .map(|row| (row.dimension(0).to_string(), row.metric(0)))
        .collect();

    Ok(status_payload(
        &totals,
        &prior,
        &daily,
        current.rows.is_empty() && current.totals.is_empty(),
    ))
}

/// Shared by the live server and the demo, so both answer in one shape.
fn status_payload(totals: &[f64], prior: &[f64], daily: &[(String, f64)], empty: bool) -> Value {
    let metrics: Vec<Value> = OVERVIEW
        .iter()
        .enumerate()
        .map(|(i, metric)| {
            let now = totals.get(i).copied().unwrap_or(0.0);
            let before = prior.get(i).copied().unwrap_or(0.0);
            json!({
                "metric": metric.api,
                "label": metric.plain,
                "unit": unit_of(metric.kind),
                "value": now,
                "previous": before,
                "change_pct": change_pct(now, before),
            })
        })
        .collect();

    let series: Vec<f64> = daily.iter().map(|(_, users)| *users).collect();
    let unlocked = achievements::unlocked(&achievements::Snapshot {
        users: totals.first().copied().unwrap_or(0.0),
        prev_users: prior.first().copied().unwrap_or(0.0),
        sessions: totals.get(1).copied().unwrap_or(0.0),
        views: totals.get(2).copied().unwrap_or(0.0),
        conversions: totals.get(3).copied().unwrap_or(0.0),
        prev_conversions: prior.get(3).copied().unwrap_or(0.0),
        bounce_rate: totals.get(4).copied().unwrap_or(0.0),
        prev_bounce_rate: prior.get(4).copied().unwrap_or(0.0),
        avg_duration: totals.get(5).copied().unwrap_or(0.0),
        daily_users: series,
    });

    json!({
        "has_data": !empty,
        "metrics": metrics,
        "daily_users": daily.iter().map(|(date, users)| json!({
            "date": iso_date(date),
            "users": users,
        })).collect::<Vec<_>>(),
        "achievements": unlocked.iter().map(|a| json!({
            "title": a.title,
            "detail": a.detail,
        })).collect::<Vec<_>>(),
    })
}

/// One dimension, ranked by one metric, optionally narrowed to rows containing
/// a substring — every `list_*` and `search_*` tool but `list_events` and
/// `list_properties` is this function with different arguments.
async fn ranked(
    ga: &Ga,
    property: &str,
    days: u32,
    limit: i64,
    dimension: &str,
    metric: &str,
    query: Option<&str>,
) -> Result<Value> {
    let mut request = ReportRequest::new(&[metric])
        .by(&[dimension])
        .range(DateRange::last_days(days))
        .top(metric, limit as i32);
    if let Some(query) = query {
        request = request.containing(dimension, query);
    }

    let report = ga.report(property, request).await?;
    let rows: Vec<(String, f64)> = report
        .rows
        .iter()
        .map(|row| (row.dimension(0).to_string(), row.metric(0)))
        .collect();

    let mut payload = ranked_payload(dimension, metric, &rows);
    if let Some(query) = query {
        payload["query"] = json!(query);
    }
    Ok(payload)
}

fn ranked_payload(dimension: &str, metric: &str, rows: &[(String, f64)]) -> Value {
    // The share is of what came back, not of the site: a top-ten list is a
    // slice, and a percentage that silently means something else is worse than
    // no percentage at all.
    let total: f64 = rows.iter().map(|(_, value)| value).sum();
    json!({
        "dimension": dimension,
        "metric": metric,
        "returned_total": total,
        "rows": rows.iter().map(|(name, value)| json!({
            "name": name,
            "value": value,
            "share_of_returned": if total > 0.0 { Some(value / total) } else { None },
        })).collect::<Vec<_>>(),
    })
}

/// Events want two answers at once — which ones fire, and whether they are
/// firing more than they were — so this one doesn't fit `ranked`.
async fn list_events(ga: &Ga, property: &str, days: u32, limit: i64) -> Result<Value> {
    let by_day = |range| {
        ReportRequest::new(&["eventCount"])
            .by(&["date"])
            .range(range)
    };

    let (top, current, previous) = tokio::try_join!(
        ga.report(
            property,
            ReportRequest::new(&["eventCount"])
                .by(&["eventName"])
                .range(DateRange::last_days(days))
                .top("eventCount", limit as i32)
        ),
        ga.report(property, by_day(DateRange::last_days(days))),
        ga.report(property, by_day(DateRange::previous_days(days)))
    )?;

    let rows: Vec<(String, f64)> = top
        .rows
        .iter()
        .map(|row| (row.dimension(0).to_string(), row.metric(0)))
        .collect();

    let series = |report: &crate::ga::Report| -> Vec<(String, f64)> {
        let mut rows: Vec<(String, f64)> = report
            .rows
            .iter()
            .map(|row| (row.dimension(0).to_string(), row.metric(0)))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    };

    Ok(events_payload(&rows, &series(&current), &series(&previous)))
}

fn events_payload(
    rows: &[(String, f64)],
    current: &[(String, f64)],
    previous: &[(String, f64)],
) -> Value {
    let total: f64 = current.iter().map(|(_, count)| count).sum();
    let before: f64 = previous.iter().map(|(_, count)| count).sum();

    let mut payload = ranked_payload("eventName", "eventCount", rows);
    payload["total_events"] = json!(total);
    payload["total_events_previous_period"] = json!(before);
    payload["change_pct"] = json!(change_pct(total, before));
    payload["daily"] = json!(current
        .iter()
        .map(|(date, count)| json!({ "date": iso_date(date), "events": count }))
        .collect::<Vec<_>>());
    payload
}

async fn live_visitors(ga: &Ga, property: &str) -> Result<Value> {
    let report = ga
        .realtime(
            property,
            ReportRequest::new(&["activeUsers"])
                .by(&["country"])
                .top("activeUsers", 30),
        )
        .await?;

    let rows: Vec<(String, f64)> = report
        .rows
        .iter()
        .map(|row| (row.dimension(0).to_string(), row.metric(0)))
        .collect();
    Ok(live_payload(&rows))
}

fn live_payload(rows: &[(String, f64)]) -> Value {
    // A dimensioned realtime request comes back without aggregates, so the
    // total is the sum of what arrived.
    let total: f64 = rows.iter().map(|(_, users)| users).sum();
    json!({
        "metric": "activeUsers",
        "active_users": total,
        "by_country": rows.iter().map(|(name, users)| json!({
            "name": name,
            "value": users,
        })).collect::<Vec<_>>(),
    })
}

// -------------------------------------------------------------- arguments ---

fn days_of(args: &Value) -> u32 {
    args.get("days")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_DAYS as i64)
        .clamp(1, 365) as u32
}

fn limit_of(args: &Value) -> i64 {
    args.get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, 100)
}

fn query_of(args: &Value) -> Result<String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if query.is_empty() {
        bail!("this tool needs a non-empty `query` to match against");
    }
    Ok(query.to_string())
}

fn unit_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Count => "count",
        Kind::Ratio => "ratio (0-1)",
        Kind::Duration => "seconds",
    }
}

/// `None` rather than infinity when there is no baseline to grow from.
fn change_pct(now: f64, before: f64) -> Option<f64> {
    if before == 0.0 {
        return None;
    }
    Some((now - before) / before * 100.0)
}

/// `YYYYMMDD` is what GA returns and `YYYY-MM-DD` is what everything else
/// reads, this server's readers included.
fn iso_date(raw: &str) -> String {
    if raw.len() == 8 && raw.chars().all(|c| c.is_ascii_digit()) {
        return format!("{}-{}-{}", &raw[0..4], &raw[4..6], &raw[6..8]);
    }
    raw.to_string()
}

// ------------------------------------------------------------------- demo ---

/// Synthetic answers for `craft mcp --demo`, shaped like the small site having
/// a good week that `craft dash --demo` shows. Fixed rather than random: the
/// demo is what the tests run against, and a moving number is not a fixture.
mod demo {
    use super::*;

    const PROPERTY: &str = "demo";
    const NAME: &str = "Contoso Labs (demo)";

    /// Same order as `theme::OVERVIEW`: users, sessions, views, key events,
    /// bounce rate, average session duration.
    const TOTALS: [f64; 6] = [12_481.0, 18_203.0, 41_776.0, 312.0, 0.412, 214.0];
    const PREVIOUS: [f64; 6] = [11_450.0, 17_004.0, 39_210.0, 258.0, 0.478, 191.0];
    const DAILY_USERS: [f64; 7] = [1402.0, 1288.0, 1531.0, 1495.0, 1760.0, 1834.0, 1971.0];
    /// A week that sags at the weekend and climbs into Monday, against one that
    /// doesn't — so the comparison has something to show.
    const DAILY_EVENTS: [f64; 7] = [6189.0, 7305.0, 5479.0, 3348.0, 4464.0, 8827.0, 10_146.0];
    const DAILY_EVENTS_BEFORE: [f64; 7] = [5783.0, 4870.0, 6088.0, 3855.0, 3044.0, 6696.0, 7204.0];

    const PAGES: [(&str, f64); 8] = [
        ("/", 6714.0),
        ("/pricing", 3917.0),
        ("/docs/quickstart", 2765.0),
        ("/blog/mining-metrics", 2136.0),
        ("/changelog", 1741.0),
        ("/docs/api", 1469.0),
        ("/about", 1270.0),
        ("/login", 1119.0),
    ];

    const EVENTS: [(&str, f64); 8] = [
        ("page_view", 14_200.0),
        ("user_engagement", 9800.0),
        ("scroll", 7450.0),
        ("session_start", 6120.0),
        ("click", 4310.0),
        ("form_submit", 2140.0),
        ("file_download", 1050.0),
        ("sign_up", 688.0),
    ];

    const REFERRERS: [(&str, f64); 7] = [
        ("(direct)", 8240.0),
        ("https://news.ycombinator.com/", 3120.0),
        ("https://www.google.com/", 2870.0),
        ("https://github.com/", 1940.0),
        ("https://x.com/", 1210.0),
        ("https://www.reddit.com/r/rust/", 980.0),
        ("https://lobste.rs/", 640.0),
    ];

    const SOURCES: [(&str, f64); 7] = [
        ("google / organic", 7420.0),
        ("(direct) / (none)", 4980.0),
        ("news.ycombinator.com / referral", 2130.0),
        ("github.com / referral", 1460.0),
        ("x.com / referral", 890.0),
        ("reddit.com / referral", 720.0),
        ("newsletter / email", 603.0),
    ];

    const COUNTRIES: [(&str, f64); 10] = [
        ("United States", 4244.0),
        ("India", 1747.0),
        ("Germany", 1123.0),
        ("United Kingdom", 998.0),
        ("Brazil", 874.0),
        ("Japan", 749.0),
        ("Canada", 624.0),
        ("Australia", 499.0),
        ("Nigeria", 499.0),
        ("France", 374.0),
    ];

    const LIVE: [(&str, f64); 10] = [
        ("United States", 44.0),
        ("India", 18.0),
        ("Germany", 12.0),
        ("United Kingdom", 10.0),
        ("Brazil", 9.0),
        ("Japan", 8.0),
        ("Canada", 6.0),
        ("Australia", 5.0),
        ("Nigeria", 5.0),
        ("France", 4.0),
    ];

    pub fn tool(name: &str, args: &Value, days: u32, limit: i64) -> Result<Value> {
        if name == "list_properties" {
            return Ok(json!({
                "properties": [{
                    "property": PROPERTY,
                    "name": NAME,
                    "account": "Anacraft demo",
                    "is_default": true,
                }],
                "note": "synthetic data — run `craft login` to connect a real property",
            }));
        }

        let payload = match name {
            "site_status" => status_payload(&TOTALS, &PREVIOUS, &daily(&DAILY_USERS, days), false),
            "live_visitors" => live_payload(&take(&LIVE, LIVE.len() as i64)),
            "list_pages" => ranked_payload("pagePath", "screenPageViews", &take(&PAGES, limit)),
            "list_referrers" => {
                ranked_payload("pageReferrer", "sessions", &take(&REFERRERS, limit))
            }
            "list_traffic_sources" => {
                ranked_payload("sessionSourceMedium", "sessions", &take(&SOURCES, limit))
            }
            "list_countries" => ranked_payload("country", "totalUsers", &take(&COUNTRIES, limit)),
            "list_events" => events_payload(
                &take(&EVENTS, limit),
                &daily(&DAILY_EVENTS, days),
                &daily(&DAILY_EVENTS_BEFORE, days),
            ),
            "search_pages" => search("pagePath", "screenPageViews", &PAGES, args, limit)?,
            "search_events" => search("eventName", "eventCount", &EVENTS, args, limit)?,
            other => bail!("no tool called {other}"),
        };

        let window = if name == "live_visitors" {
            None
        } else {
            Some(days)
        };
        let mut out = envelope(PROPERTY, Some(NAME), window, payload);
        out["synthetic"] = json!(true);
        Ok(out)
    }

    fn take(rows: &[(&str, f64)], limit: i64) -> Vec<(String, f64)> {
        rows.iter()
            .take(limit as usize)
            .map(|(name, value)| (name.to_string(), *value))
            .collect()
    }

    fn search(
        dimension: &str,
        metric: &str,
        rows: &[(&str, f64)],
        args: &Value,
        limit: i64,
    ) -> Result<Value> {
        let query = query_of(args)?;
        let needle = query.to_lowercase();
        let hits: Vec<(String, f64)> = rows
            .iter()
            .filter(|(name, _)| name.to_lowercase().contains(&needle))
            .take(limit as usize)
            .map(|(name, value)| (name.to_string(), *value))
            .collect();

        let mut payload = ranked_payload(dimension, metric, &hits);
        payload["query"] = json!(query);
        Ok(payload)
    }

    /// The fixed week, stretched or trimmed to the window that was asked for
    /// and dated so it ends yesterday, the way a real report does.
    fn daily(shape: &[f64; 7], days: u32) -> Vec<(String, f64)> {
        let yesterday = chrono::Local::now().date_naive() - chrono::Duration::days(1);
        (0..days as i64)
            .rev()
            .enumerate()
            .map(|(i, back)| {
                let date = yesterday - chrono::Duration::days(back);
                (date.format("%Y%m%d").to_string(), shape[i % shape.len()])
            })
            .collect()
    }
}

// ---------------------------------------------------------------- install ---

/// The block a client spawns us with. `demo` rides along as an argument rather
/// than as a second server: one `anacraft` entry, two modes, so re-running
/// `--install` without `--demo` upgrades it in place instead of leaving a
/// synthetic twin behind for an assistant to pick the wrong one of.
fn server_entry(demo: bool) -> Value {
    // An absolute path, not the bare command: a desktop app is not launched
    // from a shell, so it inherits a minimal PATH and frequently cannot find a
    // binary that works perfectly well in a terminal.
    let command = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "craft".to_string());
    let args = if demo {
        json!(["mcp", "--demo"])
    } else {
        json!(["mcp"])
    };
    json!({ "command": command, "args": args })
}

/// Write the server into Claude Desktop's config, leaving anything else in
/// there alone.
pub fn install(demo: bool) -> Result<()> {
    use crate::render::{bold, dim, paint};
    use crate::theme::ore;

    let entry = server_entry(demo);

    let path = claude_desktop_config()?;
    let block = serde_json::to_string_pretty(&json!({
        "mcpServers": { "anacraft": entry.clone() }
    }))?;

    if !path.parent().is_some_and(|dir| dir.exists()) {
        println!(
            "\n  {} Claude Desktop's config directory isn't here:\n  {}\n",
            paint("⛏", ore::redstone()),
            dim(&path.display().to_string()),
        );
        println!("  add this to your MCP client's config yourself:\n\n{block}\n");
        return Ok(());
    }

    let mut config =
        read_client_config(&path, &format!("add the anacraft block by hand:\n{block}"))?;

    let root = config
        .as_object_mut()
        .with_context(|| format!("{} isn't a JSON object", path.display()))?;
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`mcpServers` isn't a JSON object")?;
    let replaced = servers.insert("anacraft".into(), entry).is_some();

    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )
    .with_context(|| format!("writing {}", path.display()))?;

    println!(
        "\n  {} anacraft {} in {}\n",
        paint("✓", ore::emerald()),
        if replaced { "updated" } else { "added" },
        dim(&path.display().to_string()),
    );
    println!("  restart {} to pick it up\n", bold("Claude Desktop"));

    if demo {
        // Loud, because the numbers are designed to look plausible: this is the
        // one mode where a confident answer is entirely invented.
        println!(
            "  {} serving {} — every answer is made up, and marked\n     \
             `synthetic: true`. Re-run without {} once the account is live.\n",
            paint("·", ore::iron()),
            bold("synthetic data"),
            bold("--demo"),
        );
        return Ok(());
    }

    // Wiring the server up is not the same as being able to serve. Say what is
    // still missing here, where there is a terminal to read it in, rather than
    // leaving the user to meet it as a locked tool inside the client.
    match Config::load() {
        // Installing is not serving, so this leans on the cached flag rather
        // than going out to Supabase: the next `craft mcp` refreshes it.
        Ok(cfg) => {
            if let Err(reason) = unlock(cfg.supporter) {
                println!("  {} {reason}\n", paint("·", ore::iron()));
            }
        }
        Err(err) => println!(
            "  {} could not read the config: {err}\n",
            paint("·", ore::iron())
        ),
    }
    Ok(())
}

/// The client's config as JSON, or an empty object if there isn't one yet.
/// Refusing beats rewriting: this file is the user's, and other MCP servers
/// live in it, so unparseable JSON stops us with `hint` rather than costing
/// someone the rest of their servers.
fn read_client_config(path: &Path, hint: &str) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&raw)
        .with_context(|| format!("{} isn't valid JSON — {hint}", path.display()))
}

/// Lift our one key out of a client config, reporting whether it was there.
/// Anything shaped unexpectedly — no `mcpServers`, not an object — is simply a
/// config without us in it, not an error to stop over.
fn drop_server(config: &mut Value) -> bool {
    config
        .as_object_mut()
        .and_then(|root| root.get_mut("mcpServers"))
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.remove("anacraft"))
        .is_some()
}

/// Take the server back out of Claude Desktop's config. The inverse of
/// `install`, and just as narrow: one key leaves, everything else — other
/// servers, unrelated settings — stays exactly as it was found.
pub fn uninstall() -> Result<()> {
    use crate::render::{bold, dim, paint};
    use crate::theme::ore;

    let path = claude_desktop_config()?;
    let mut config =
        read_client_config(&path, "remove the anacraft block from `mcpServers` by hand")?;

    let removed = drop_server(&mut config);

    if !removed {
        // Nothing to undo is not a failure — someone tidying up after a
        // reinstall, or a config that was never written, both land here.
        println!(
            "\n  {} no anacraft server in {}\n",
            paint("·", ore::iron()),
            dim(&path.display().to_string()),
        );
        return Ok(());
    }

    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )
    .with_context(|| format!("writing {}", path.display()))?;

    println!(
        "\n  {} anacraft removed from {}\n",
        paint("✓", ore::emerald()),
        dim(&path.display().to_string()),
    );
    println!("  restart {} to drop the tools\n", bold("Claude Desktop"));
    Ok(())
}

fn claude_desktop_config() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not locate a home directory")?;

    // `cfg!` rather than `#[cfg]`: every branch stays compiled everywhere, so a
    // path that is wrong on Windows fails to build on Linux too.
    let dir = if cfg!(target_os = "windows") {
        dirs::config_dir()
            .unwrap_or_else(|| home.join("AppData").join("Roaming"))
            .join("Claude")
    } else if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("Claude")
    } else {
        match std::env::var_os("XDG_CONFIG_HOME") {
            Some(raw) if !raw.is_empty() => PathBuf::from(raw),
            _ => home.join(".config"),
        }
        .join("Claude")
    };

    Ok(dir.join("claude_desktop_config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server {
            cfg: Config::default(),
            source: Source::Demo,
            property: None,
            cache: HashMap::new(),
        }
    }

    fn request(id: i64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    async fn call(server: &mut Server, name: &str, args: Value) -> Value {
        let reply = server
            .dispatch(request(
                1,
                "tools/call",
                json!({ "name": name, "arguments": args }),
            ))
            .await
            .expect("a request always gets an answer");
        reply["result"].clone()
    }

    /// The structured half of a tool result, which is what an assistant reads.
    async fn payload(server: &mut Server, name: &str, args: Value) -> Value {
        let result = call(server, name, args).await;
        assert_eq!(result["isError"], json!(false), "tool failed: {result}");
        result["structuredContent"].clone()
    }

    #[tokio::test]
    async fn initialize_speaks_the_clients_revision_when_we_know_it() {
        let mut server = server();
        let reply = server
            .dispatch(request(
                1,
                "initialize",
                json!({ "protocolVersion": "2024-11-05" }),
            ))
            .await
            .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], json!("2024-11-05"));
        assert_eq!(reply["result"]["serverInfo"]["name"], json!("anacraft"));
    }

    #[tokio::test]
    async fn initialize_falls_back_to_ours_for_a_revision_we_do_not() {
        let mut server = server();
        let reply = server
            .dispatch(request(
                1,
                "initialize",
                json!({ "protocolVersion": "1999-01-01" }),
            ))
            .await
            .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], json!(PROTOCOL_VERSION));
    }

    #[tokio::test]
    async fn the_handshake_carries_the_mark_for_a_client_that_can_draw_it() {
        let mut server = server();
        let reply = server
            .dispatch(request(
                1,
                "initialize",
                json!({ "protocolVersion": ICONS_SINCE }),
            ))
            .await
            .unwrap();
        let icon = &reply["result"]["serverInfo"]["icons"][0];
        assert_eq!(icon["mimeType"], json!("image/png"));
        let src = icon["src"].as_str().expect("an icon has a src");
        // A client is allowed to refuse anything that isn't https or `data:`,
        // and one that fetches this must never leave the machine to do it.
        assert!(src.starts_with("data:image/png;base64,"), "{src}");
        let bytes = STANDARD
            .decode(src.trim_start_matches("data:image/png;base64,"))
            .expect("the src decodes");
        assert_eq!(bytes, ICON_PNG);
    }

    #[tokio::test]
    async fn an_older_client_is_not_sent_an_icon_it_has_no_field_for() {
        let mut server = server();
        let reply = server
            .dispatch(request(
                1,
                "initialize",
                json!({ "protocolVersion": "2024-11-05" }),
            ))
            .await
            .unwrap();
        assert!(reply["result"]["serverInfo"]["icons"].is_null());
    }

    #[tokio::test]
    async fn a_notification_gets_no_answer() {
        // Anything written to stdout here would land in the middle of the
        // client's message stream.
        let mut server = server();
        let quiet = server
            .dispatch(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        assert!(quiet.is_none());
    }

    #[tokio::test]
    async fn an_unknown_method_is_a_protocol_error() {
        let mut server = server();
        let reply = server
            .dispatch(request(7, "resources/list", json!({})))
            .await;
        assert_eq!(reply.unwrap()["error"]["code"], json!(-32601));
    }

    #[tokio::test]
    async fn every_tool_is_listed_with_a_schema() {
        let mut server = server();
        let reply = server
            .dispatch(request(1, "tools/list", json!({})))
            .await
            .unwrap();
        let listed = reply["result"]["tools"].as_array().unwrap().clone();

        assert_eq!(listed.len(), TOOLS.len());
        for tool in &listed {
            assert!(tool["inputSchema"]["properties"].is_object(), "{tool}");
            assert_eq!(tool["annotations"]["readOnlyHint"], json!(true), "{tool}");
        }

        // The search tools are the only ones that demand an argument.
        for tool in &listed {
            let required = tool["inputSchema"]["required"].as_array().unwrap();
            let name = tool["name"].as_str().unwrap();
            assert_eq!(
                required.contains(&json!("query")),
                name.starts_with("search_"),
                "{name} required {required:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_tool_is_rejected() {
        let mut server = server();
        let reply = server
            .dispatch(request(1, "tools/call", json!({ "name": "drop_property" })))
            .await
            .unwrap();
        assert_eq!(reply["error"]["code"], json!(-32602));
    }

    #[tokio::test]
    async fn site_status_says_which_property_and_which_days() {
        let mut server = server();
        let out = payload(&mut server, "site_status", json!({ "days": 14 })).await;

        assert_eq!(out["property"], json!("demo"));
        assert_eq!(out["date_range"]["days"], json!(14));
        assert_eq!(out["date_range"]["start_date"], json!("14daysAgo"));
        assert_eq!(out["daily_users"].as_array().unwrap().len(), 14);

        // Labelled numbers with units, not rendered panels.
        let metrics = out["metrics"].as_array().unwrap();
        assert_eq!(metrics.len(), OVERVIEW.len());
        assert_eq!(metrics[0]["metric"], json!("totalUsers"));
        assert_eq!(metrics[0]["label"], json!("users"));
        assert_eq!(metrics[4]["unit"], json!("ratio (0-1)"));
    }

    #[tokio::test]
    async fn site_status_carries_the_achievements_the_dashboard_would_show() {
        let mut server = server();
        let out = payload(&mut server, "site_status", json!({})).await;
        let titles: Vec<&str> = out["achievements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["title"].as_str().unwrap())
            .collect();
        assert!(titles.contains(&"Long Haul"), "got {titles:?}");
    }

    #[tokio::test]
    async fn live_visitors_is_a_realtime_window_not_a_date_range() {
        let mut server = server();
        let out = payload(&mut server, "live_visitors", json!({})).await;

        assert!(out["date_range"].is_null());
        assert_eq!(out["window"], json!("the last 30 minutes"));
        assert_eq!(out["active_users"], json!(121.0));
    }

    #[tokio::test]
    async fn the_ranked_tools_answer_in_one_shape() {
        let mut server = server();
        for tool in [
            "list_pages",
            "list_events",
            "list_referrers",
            "list_traffic_sources",
            "list_countries",
        ] {
            let out = payload(&mut server, tool, json!({})).await;
            let rows = out["rows"]
                .as_array()
                .unwrap_or_else(|| panic!("{tool}: no rows"));
            assert!(!rows.is_empty(), "{tool} came back empty");
            assert!(rows[0]["name"].is_string(), "{tool}: {}", rows[0]);
            assert!(rows[0]["value"].is_number(), "{tool}: {}", rows[0]);
            assert!(out["dimension"].is_string(), "{tool} named no dimension");
        }
    }

    #[tokio::test]
    async fn list_events_compares_the_period_with_the_one_before_it() {
        let mut server = server();
        let out = payload(&mut server, "list_events", json!({})).await;

        assert_eq!(out["dimension"], json!("eventName"));
        assert_eq!(out["daily"].as_array().unwrap().len(), 7);
        assert!(
            out["total_events"].as_f64().unwrap()
                > out["total_events_previous_period"].as_f64().unwrap()
        );
        assert!(out["change_pct"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn search_matches_a_substring_and_says_what_it_searched_for() {
        let mut server = server();
        let out = payload(&mut server, "search_pages", json!({ "query": "DOCS" })).await;

        let names: Vec<&str> = out["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["/docs/quickstart", "/docs/api"]);
        assert_eq!(out["query"], json!("DOCS"));
    }

    #[tokio::test]
    async fn a_search_without_a_query_comes_back_as_tool_output_not_a_transport_fault() {
        let mut server = server();
        let result = call(&mut server, "search_events", json!({ "query": "  " })).await;

        assert_eq!(result["isError"], json!(true));
        assert!(result["structuredContent"]["error"]
            .as_str()
            .unwrap()
            .contains("query"));
    }

    #[tokio::test]
    async fn limits_and_windows_are_clamped_rather_than_trusted() {
        let mut server = server();
        let out = payload(&mut server, "list_pages", json!({ "limit": 9999 })).await;
        assert_eq!(out["rows"].as_array().unwrap().len(), demo_pages());

        let out = payload(&mut server, "site_status", json!({ "days": 0 })).await;
        assert_eq!(out["date_range"]["days"], json!(1));
    }

    fn demo_pages() -> usize {
        8 // every synthetic page, since the clamp lands well above the fixture
    }

    #[tokio::test]
    async fn the_same_question_twice_is_answered_from_memory() {
        let mut server = server();
        let first = payload(&mut server, "list_countries", json!({})).await;
        assert!(first["cached"].is_null());

        let second = payload(&mut server, "list_countries", json!({})).await;
        assert_eq!(second["cached"], json!(true));

        // A different window is a different question.
        let other = payload(&mut server, "list_countries", json!({ "days": 30 })).await;
        assert!(other["cached"].is_null());
    }

    #[test]
    fn installing_the_demo_writes_the_flag_the_server_reads() {
        let live = server_entry(false);
        assert_eq!(live["args"], json!(["mcp"]), "got {live}");

        let demo = server_entry(true);
        assert_eq!(demo["args"], json!(["mcp", "--demo"]), "got {demo}");

        // Same key, same command: `--install` twice is an update, not a pair of
        // servers offering the same ten tools over different data.
        assert_eq!(live["command"], demo["command"]);
    }

    #[test]
    fn uninstalling_takes_one_key_and_nothing_else() {
        let mut config = json!({
            "mcpServers": { "anacraft": { "command": "craft" }, "other": { "command": "x" } },
            "theme": "dark",
        });
        assert!(drop_server(&mut config));
        assert!(config["mcpServers"]["anacraft"].is_null());
        assert_eq!(config["mcpServers"]["other"]["command"], json!("x"));
        assert_eq!(config["theme"], json!("dark"), "got {config}");

        // Twice is not an error, and neither is a config that never had us.
        assert!(!drop_server(&mut config));
        assert!(!drop_server(&mut json!({})));
        assert!(!drop_server(&mut json!({ "mcpServers": "nonsense" })));
    }

    /// The demo's numbers are invented, so the handshake says so before an
    /// assistant can quote one.
    #[tokio::test]
    async fn the_demo_handshake_admits_the_data_is_synthetic() {
        let mut server = server();
        let reply = server
            .dispatch(request(1, "initialize", json!({})))
            .await
            .expect("a handshake is answered");
        let instructions = reply["result"]["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains("synthetic"), "got {instructions}");
    }

    #[test]
    fn the_gate_wants_a_subscription() {
        let err = subscription(false).unwrap_err();
        assert!(err.contains("craft subscribe"), "got {err}");
        assert!(
            err.contains("--demo"),
            "no way out for a non-subscriber: {err}"
        );
        assert!(
            err.contains("craft login"),
            "a subscriber on a new machine is left guessing: {err}"
        );

        assert!(subscription(true).is_ok());
    }

    fn locked_server() -> Server {
        Server {
            cfg: Config::default(),
            source: Source::Locked(subscription(false).unwrap_err()),
            property: None,
            cache: HashMap::new(),
        }
    }

    /// The gate must never take the process down with it: a client reads an
    /// early exit as "server disconnected", which points at the pipes instead
    /// of the subscription.
    #[tokio::test]
    async fn a_locked_server_still_shakes_hands_and_lists_its_tools() {
        let mut server = locked_server();

        let reply = server
            .dispatch(request(1, "initialize", json!({})))
            .await
            .expect("a handshake is answered");
        assert!(reply["error"].is_null(), "handshake failed: {reply}");
        let instructions = reply["result"]["instructions"].as_str().unwrap_or_default();
        assert!(
            instructions.contains("craft subscribe"),
            "the handshake keeps the reason to itself: {instructions}"
        );

        let listed = server
            .dispatch(request(2, "tools/list", json!({})))
            .await
            .expect("a request is answered");
        assert_eq!(
            listed["result"]["tools"].as_array().map(Vec::len),
            Some(TOOLS.len()),
            "a locked server hid its tools: {listed}"
        );
    }

    #[tokio::test]
    async fn a_locked_tool_call_reads_as_a_tool_error_not_a_broken_server() {
        let mut server = locked_server();
        let result = call(&mut server, "site_status", json!({})).await;

        assert_eq!(result["isError"], json!(true), "got {result}");
        let error = result["structuredContent"]["error"]
            .as_str()
            .unwrap_or_default();
        assert!(error.contains("craft subscribe"), "got {error}");
        assert!(
            !error.contains('\n'),
            "terminal wrapping leaked into JSON: {error}"
        );
    }

    #[test]
    fn a_missing_baseline_is_no_change_rather_than_infinite_growth() {
        assert_eq!(change_pct(100.0, 50.0), Some(100.0));
        assert_eq!(change_pct(100.0, 0.0), None);
    }

    #[test]
    fn ga_dates_are_rewritten_the_way_everything_else_reads_them() {
        assert_eq!(iso_date("20260819"), "2026-08-19");
        assert_eq!(iso_date("yesterday"), "yesterday");
    }
}
