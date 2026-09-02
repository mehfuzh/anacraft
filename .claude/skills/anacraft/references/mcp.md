# `craft mcp` — the MCP server

Stdio only: the client spawns `craft mcp` as a child process and talks
newline-delimited JSON-RPC over its pipes. No port, no listener, nothing to
firewall. Stdout belongs to the protocol, so everything human-facing goes to
stderr — never pipe stdout anywhere while the server is running.

Speaks MCP revisions `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05`,
echoing back whichever the client asks for. The tool surface is the same in all
four. A client on `2025-11-25` also gets the anacraft mark in the handshake, as
an inline `data:` PNG on `serverInfo.icons`, so a connector list can draw it
without fetching anything; older revisions have no field for it and are not
sent one. Whether the icon is drawn is the client's call — some show a letter
placeholder regardless.

## Wiring it up

**Claude Desktop** — `craft mcp --install` writes the block and merges with
whatever else is in the file; add `--demo` to write a block that serves
synthetic data. Either way it is the one `anacraft` entry, so re-running
`--install` without `--demo` upgrades it in place rather than leaving a
synthetic twin alongside the real one. Restart the app afterwards. To do it by
hand:

| Platform | File |
|---|---|
| macOS | `~/Library/Application Support/Claude/claude_desktop_config.json` |
| Linux | `~/.config/Claude/claude_desktop_config.json` |
| Windows | `%APPDATA%\Claude\claude_desktop_config.json` |

```json
{
  "mcpServers": {
    "anacraft": { "command": "/usr/local/bin/craft", "args": ["mcp"] }
  }
}
```

Absolute path, deliberately: a desktop app is not launched from a shell and
does not inherit the `PATH` where `craft` works fine. `which craft` gives the
value to paste.

**Claude Code** — `claude mcp add anacraft -- craft mcp`.

**Anything else** — same command, same args. Add `--demo` to any of them to
serve synthetic data with no account and no subscription, which is the fastest
way to prove the client-side wiring before blaming credentials. The demo says so
at the handshake and stamps `synthetic: true` on every answer, so an assistant
reading it knows not to quote the numbers as real.

## Before it will serve

Two preconditions, both checked at startup so the reason is known before the
first tool call:

1. **A subscription.** `supporter = true` in `~/.config/anacraft/config.toml`,
   after `craft subscribe`. `--demo` skips this.
2. **A stored token.** `craft login`, run in a terminal by a person. The server
   will not start an OAuth flow — a browser consent screen inside a client's
   subprocess is not something an agent can complete, and read-only means
   read-only.

Neither is fatal. Missing one **locks** the server rather than exiting it: the
reason goes to stderr — the client's log — the handshake still succeeds, the
tools are still listed, and every call comes back as a tool error carrying that
reason. This is deliberate. A process that exits during startup reaches the user
as `Server disconnected`, which points at the pipes instead of the subscription;
a locked server tells the assistant what to say. The handshake `instructions`
carry the reason too, so it can be relayed before anything is called.

## The tools

Every tool takes an optional `property` (numeric GA4 id) and falls back to the
saved default, so an assistant that knows nothing about the config still gets
answers. `days` defaults to 7 and is clamped to 1–365; `limit` defaults to 10
and is clamped to 1–100.

| Tool | Arguments | Answers |
|---|---|---|
| `site_status` | `days` | Headline metrics against the period before, the daily user series, and the achievements that fired |
| `live_visitors` | — | Active users in the last 30 minutes, by country |
| `list_pages` | `days`, `limit` | Most-visited pages, by views |
| `list_events` | `days`, `limit` | Events by count, plus the per-day total against the previous period |
| `list_referrers` | `days`, `limit` | The URLs sending traffic, by sessions |
| `list_traffic_sources` | `days`, `limit` | GA4 source / medium pairs, by sessions |
| `list_countries` | `days`, `limit` | Countries, by users |
| `list_properties` | — | Every property this account can read, and which is default |
| `search_pages` | `query`\*, `days`, `limit` | Pages whose path contains a substring |
| `search_events` | `query`\*, `days`, `limit` | Events whose name contains a substring |

\* required; the match is case-insensitive.

Which tool answers which question:

- "How is the site doing?" · "Are we up or down this week?" → `site_status`
- "Who's on it right now?" → `live_visitors`
- "Which pages are doing well?" · "How did the blog do?" → `list_pages`,
  then `search_pages` with `/blog`
- "Where is the traffic coming from?" → `list_traffic_sources` for the channel
  mix, `list_referrers` for the actual links
- "Did the signup flow get used?" → `search_events` with `signup`

## What comes back

Structured JSON, not the rendered panels — labelled numbers with their units.
Every answer names the property and the window it covers, so quote the window
alongside any number taken from it.

```json
{
  "property": "397412345",
  "property_name": "anacraft.dev",
  "date_range": {
    "start_date": "7daysAgo",
    "end_date": "yesterday",
    "days": 7,
    "note": "GA4 relative dates; the window ends yesterday, because today is still partial"
  }
}
```

On top of that envelope:

- **`site_status`** — `has_data`, `metrics[]` (`metric`, `label`, `unit`,
  `value`, `previous`, `change_pct`), `daily_users[]` (`date`, `users`),
  `achievements[]` (`title`, `detail`).
- **the ranked tools** — `dimension`, `metric`, `returned_total`, and `rows[]`
  of `name` / `value` / `share_of_returned`. That share is of the rows returned,
  not of the site: a top-ten list is a slice, and a percentage that quietly
  means something else is worse than none.
- **`list_events`** — the ranked shape plus `total_events`,
  `total_events_previous_period`, `change_pct`, and `daily[]`.
- **`live_visitors`** — `active_users` and `by_country[]`, with
  `"window": "the last 30 minutes"` in place of a date range.
- **`list_properties`** — `properties[]` only, with no property or window: it is
  not a question about one site.

Two flags worth reading:

- `"cached": true` — the same question was asked within the last minute
  (ten seconds for `live_visitors`) and this is the stored answer. Quota is
  shared with the dashboard, and an agent in a loop can out-ask a human by
  orders of magnitude. A number that will not move usually means this, not a
  site that went quiet.
- `"synthetic": true` — `--demo`. Never present these numbers as the real site.

A failed report comes back as tool output with `isError` set, not as a
transport fault, so the reason — a rejected property id, an expired login — is
readable and actionable rather than looking like a broken server.

## What it will not do

Every tool is a read. Nothing in the server writes to `~/.anacraft/`, starts an
OAuth flow, or changes the default property: `craft login` and `craft use` stay
human-only commands. An agent cannot silently repoint the tool at another
property — it can only pass `property` on a single call.
