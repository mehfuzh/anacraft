---
name: anacraft
description: Install and drive anacraft — the `craft` terminal dashboard for Google Analytics 4. Covers installing the binary, signing in and picking a property, the one-shot report commands, the dashboard's panels and keys, palettes, the config file and its environment overrides, and wiring `craft mcp` into Claude Desktop or another MCP client so an assistant can read the site's numbers. Use when the user asks how to install or use anacraft or `craft`, connect a GA4 property to it, read or configure the dashboard, set up its MCP server, or when a `craft` command fails with an error.
---

# Driving anacraft

**The command is `craft`.** `anacraft` is installed alongside it as a symlink,
so both work; the crate, the brand, and the paths under `~/.anacraft/` keep the
long name. Reports print and exit; `craft` on its own opens the dashboard.

This skill is the anacraft side. The Google side — creating the property,
installing the tag, enabling the APIs, granting access — is the
`google-analytics-setup` skill; send the user there the moment a problem turns
out to be in the GA4 console rather than in this tool.

## Start here

| Situation | Go to |
|---|---|
| Nothing installed | 1 |
| Installed, wants to see it before signing in | 2 |
| Installed, has a GA4 property | 3 |
| Signed in, wants numbers | 4 / 5 |
| Wants an assistant to read the site | 6 |
| A command failed | `references/troubleshooting.md` |

## 1. Install

```sh
curl -fsSL https://anacraft.dev/install.sh | bash
```

Goes to `/usr/local/bin` when that is writable, else `~/.local/bin` — never
with `sudo`. `INSTALL_DIR=~/bin` picks somewhere else; `VERSION=v0.6.0` pins a
release instead of taking the latest. macOS and Linux, x86_64 and arm64.

From source: `cargo install --git https://github.com/mehfuzh/anacraft` (Rust
1.74+). Or take an archive from
[Releases](https://github.com/mehfuzh/anacraft/releases) and put `craft` on the
`PATH` by hand.

Needs a terminal with truecolor and at least **80×24** — below that the
dashboard draws a resize notice instead of itself, on purpose.

## 2. Look at it without an account

```sh
craft dash --demo    # the full dashboard on synthetic data
craft demo           # just the overview panel
```

Synthetic numbers shaped like a small site having a good week. Useful for
judging a palette, for a screenshot, and for confirming the terminal can
actually render the thing before blaming the Google side of the setup. `craft`
with no property saved falls into demo mode by itself.

## 3. Connect a property

```sh
craft login          # OAuth in the browser, stores a refresh token
craft props          # every property this account can read
craft use 397412345  # save it as the default
```

`craft use` takes the **numeric property id** — `397412345`, from GA4's Admin →
Property details. Not the `G-XXXXXXXXXX` measurement id, which belongs to the
tag and will simply not be found here. That mix-up is the single most common
one; `craft props` prints the right numbers next to each name, so read the id
off that rather than out of a browser tab.

`use` **adds** rather than replaces, so the config accumulates properties and
`tab` cycles them in the dashboard.

Official builds ship an OAuth client, so `login` needs no Google Cloud setup.
The account still needs at least **Viewer** on the property, and the project
behind the client needs the **Data API** and **Admin API** enabled — both are
`google-analytics-setup` territory when they turn out to be the problem.

## 4. One-shot reports

Not everything needs a dashboard. These print and exit — the right shape for a
pipe, a cron job, or a quick answer.

```sh
craft overview --days 30   # headline metrics, deltas, achievements
craft pages                # most-visited pages
craft portals              # where traffic arrives from (source / medium)
craft realms               # traffic by country
craft live                 # who is on the site right now
```

`--days` on all but `live`; `--limit` on the three ranked ones. Two flags are
global: `--property <id>` queries something other than the default without
saving it, and `--theme <name>` renders in another palette for one run.

## 5. The dashboard

`craft` (or `craft dash`). Seven panels; hiding one gives its space back to the
rest rather than leaving a hole.

| Key | Panel | Shows |
|---|---|---|
| `1` `e` | EVENTS | Events per day, this period drawn over the last |
| `2` `l` | RIGHT NOW | Live count and a spawn / wander-off feed |
| `3` `m` | COUNTRIES | Traffic on a world map |
| `4` `p` | TOP PAGES | Pages with view bars and rank movement |
| `5` `v` | VITALS | Users, sessions, views, key events, bounce, avg. session |
| `6` `g` | TOP COUNTRIES | Ranked countries |
| `7` `d` | DAILY USERS | User trend across the period |

`t` cycles the palette and saves it · `r` forces a refetch · `tab` switches
property · `?` or `h` for help · `q` or `Esc` quits · `s` previews the
Anacrafter look, demo only.

Cadence flags: `--days`, `--refresh` (seconds between reports, default 30),
`--live-refresh` (the realtime tick, minimum 2). Each falls back to what the
property saved, then to the default.

## 6. Let an assistant read the site

`craft mcp` serves the same numbers over the Model Context Protocol, so Claude
Desktop or any MCP client can answer "how is the site doing" without a human
reading a TUI.

```sh
craft mcp --install          # merge the server into Claude Desktop's config
craft mcp --install --demo   # ...with `--demo` in the args it writes
craft mcp --demo             # synthetic data — no account, no subscription
```

Then restart Claude Desktop. `--install` leaves any other servers in that file
alone, and refuses rather than rewrites a config it cannot parse.

Three things to know before promising it will work:

- **It needs a subscription.** `craft subscribe`, then `supporter = true` in
  `config.toml`. `--demo` is ungated.
- **It needs `craft login` to have been run first**, in a terminal. The server
  is read-only by design and will not open a browser inside a client
  subprocess.
- **Neither missing one breaks the connection.** The server starts anyway and
  every tool call answers with what is missing, so a client that shows
  `Server disconnected` has a wiring problem — a wrong path, an old binary —
  not a subscription problem.
- **Use an absolute path** in any config written by hand. A desktop app is not
  launched from a shell and often cannot find a bare `craft`.

Ten tools: `site_status`, `live_visitors`, `list_pages`, `list_events`,
`list_referrers`, `list_traffic_sources`, `list_countries`, `list_properties`,
`search_pages`, `search_events`. Arguments, response shapes, and wiring for
clients other than Claude Desktop are in `references/mcp.md`.

## Palettes

```sh
craft theme                # list them, drawn in their own colors
craft theme tokyo-night    # switch and persist
craft --theme github dash  # one run only
```

`osaka-jade` (default) · `catppuccin` · `github` · `tokyo-night` ·
`catppuccin-latte`. The ore vocabulary — diamond, gold, redstone, lapis — is
mapped onto whichever palette is selected, so the texture pack survives a swap.

## Configuration

| File | Holds |
|---|---|
| `~/.config/anacraft/config.toml` | Properties and their settings. Hand-editable. |
| `~/.anacraft/token.json` | OAuth refresh token, written `0600`. |

They are split deliberately: `~/.config` ends up in dotfile repos, and a
refresh token has no business travelling with it. `$XDG_CONFIG_HOME` is
honoured. A pre-0.4 `~/.anacraft/config.json` is migrated on first run.

```toml
active    = "397412345"
theme     = "osaka-jade"   # for any property that doesn't name its own
supporter = true           # set once a subscription is active

[[property]]
id           = "397412345"
name         = "anacraft.dev"
label        = "site"      # shown instead of name in the switcher
theme        = "catppuccin"
days         = 14
refresh      = 60
live_refresh = 5

[[property]]
id = "88820011"            # everything optional: inherits the defaults
```

Every key under `[[property]]` is optional and falls back to the global
default. Command-line flags beat both.

| Variable | Does |
|---|---|
| `ANACRAFT_PROPERTY_ID` | Overrides the saved property, for keeping none on disk |
| `ANACRAFT_OAUTH_CLIENT_ID` / `_SECRET` | Use your own OAuth client instead of the built-in one |

`~/.anacraft/client.json` does the same as the two OAuth variables. Registering
your own Google Cloud project also insulates you from other people's quota.

## When something fails

Every error the tool prints names the command that fixes it — read it before
reaching for anything else. Cause-by-message, and the ones whose wording points
at the wrong culprit, are in `references/troubleshooting.md`.
