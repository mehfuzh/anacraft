<h1 align="center">⛏ craft</h1>

<p align="center"><b>Google Analytics, mined block by block.</b></p>

<p align="center">
  A terminal dashboard for Google Analytics 4 — seven live panels, ore-textured
  bars, a realtime event feed, and achievement toasts when the numbers move.
</p>

<p align="center">
  <a href="https://anacraft.dev">anacraft.dev</a> ·
  <a href="https://github.com/mehfuzh/anacraft/releases">Releases</a> ·
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache%202.0-blue.svg"></a>
  <a href="https://www.rust-lang.org/"><img alt="Rust 1.74+" src="https://img.shields.io/badge/Rust-1.74+-orange.svg"></a>
</p>

<p align="center">
  <img src="docs/dash.png" alt="The anacraft dashboard: seven panels showing GA4 metrics in a terminal" width="960">
</p>

<p align="center"><sub><code>craft dash --demo</code> · osaka-jade · 132×52</sub></p>

## Install

**macOS / Linux**

```sh
curl -fsSL https://anacraft.dev/install.sh | bash
```

Installs to `/usr/local/bin` when that is writable, otherwise `~/.local/bin` —
never with `sudo`. Set `INSTALL_DIR` to choose somewhere else:

```sh
curl -fsSL https://anacraft.dev/install.sh | INSTALL_DIR=~/bin bash
```

**From source**

```sh
cargo install --git https://github.com/mehfuzh/anacraft
```

**Manual** — grab a binary from [Releases](https://github.com/mehfuzh/anacraft/releases), extract it, and put `anacraft` on your `PATH`.

## Quick start

`anacraft` with no command opens the dashboard — `dash` is the default. With no
property saved it runs on synthetic data, so it works before you sign in.

```sh
# The dashboard, on synthetic data — no Google account needed
craft

# Connect a real GA4 property
craft login        # OAuth sign-in
craft props        # list the properties this account can read
craft use 1234567  # save it as the default

# Same bare command, now against your property
craft
```

### One-shot reports

Not everything needs a dashboard. These print and exit.

```sh
craft overview --days 30   # headline metrics, deltas, achievements
craft pages                # most-visited pages
craft portals              # where traffic arrives from
craft realms               # traffic by country
craft live                 # who is on the site right now
craft demo                 # render an overview from synthetic data
```

Two flags are global: `--property <id>` queries a property other than the saved
default, and `--theme <name>` renders with a palette other than the saved one.

## The dashboard

Seven panels, each toggleable. Turn off what you do not care about and the rest
reflows to fill the terminal.

| Key | Panel | What it shows |
|-----|-------|---------------|
| `1` | **EVENTS** | Event count per day, this period drawn over the last one, with the total and its change |
| `2` | **RIGHT NOW** | Live player count plus a spawn / wander-off event feed |
| `3` | **COUNTRIES** | Traffic plotted on a world map |
| `4` | **TOP PAGES** | Most-visited pages with view bars and rank movement |
| `5` | **VITALS** | Users, sessions, views, conversions, bounce rate, avg. session |
| `6` | **TOP COUNTRIES** | Ranked countries with tier markers |
| `7` | **DAILY USERS** | User trend across the period |

### Controls

| Key | Action |
|-----|--------|
| `1`–`7` | Toggle a panel — `e` `l` `m` `p` `v` `g` `d` do the same |
| `t` | Cycle the palette, and save it |
| `s` | Demo only — preview the Anacrafter look |
| `r` | Rebuild — force a refetch now |
| `?` / `h` | Help overlay |
| `q` / `Esc` | Quit |

## Palettes

```sh
craft theme                # list the palettes with swatches
craft theme tokyo-night    # switch and persist
craft --theme github dash  # override for one run
```

`osaka-jade` (default) · `catppuccin` · `github` · `tokyo-night` · `catppuccin-latte`

The ore vocabulary — diamond, gold, redstone, lapis — is mapped onto whichever
palette is selected, so the texture pack survives a theme swap.

The command is `craft`. `anacraft` is installed alongside it as an alias, so
older scripts and anything you have in muscle memory keep working.

## Configuration

| File | Purpose |
|------|---------|
| `~/.config/anacraft/config.toml` | Properties and their settings |
| `~/.anacraft/token.json` | OAuth refresh token, written by `login` |

Config honours `$XDG_CONFIG_HOME`. Tokens stay out of `~/.config` on purpose —
that directory ends up in dotfile repos, and a refresh token has no business
travelling with it. A pre-0.4 `~/.anacraft/config.json` is migrated on first run.

### Multiple properties

`craft use <id>` adds a property rather than replacing the last one, so the
config accumulates. In the dashboard, `tab` cycles between them.

```toml
active = "397412345"
theme  = "osaka-jade"        # palette for any property that doesn't name one

[[property]]
id           = "397412345"
name         = "anacraft.dev"
label        = "site"        # shown instead of name in the switcher
theme        = "catppuccin"
days         = 14
refresh      = 60
live_refresh = 5

[[property]]
id = "88820011"              # everything optional: inherits the defaults
```

Every key under `[[property]]` is optional and falls back to the global default,
so switching to a property that saved nothing lands on the defaults rather than
inheriting the previous property's window. Command-line flags beat both.

`ANACRAFT_PROPERTY_ID` overrides the saved property if you would rather not keep
one on disk.

### Your own OAuth client

Official builds carry one, so `craft login` works with no setup. To use your own
instead, set `ANACRAFT_OAUTH_CLIENT_ID` and `ANACRAFT_OAUTH_CLIENT_SECRET`, or
write `~/.anacraft/client.json`. Both take precedence over the built-in client,
and registering your own Google Cloud project also insulates you from other
people's quota consumption.

## Setting up GA4

The Google side — property, data stream, tag, access management, API enablement
— is documented at [anacraft.dev/setup-ga4](https://anacraft.dev/setup-ga4.html).

Cloned the repo and use [Claude Code](https://claude.com/claude-code)? The same
guide ships as a skill in `.claude/skills/google-analytics-setup/`. Ask Claude to
configure Google Analytics and it walks the console steps with you, including
tag installs per framework and what to check when nothing arrives.

## Requirements

- A Google Analytics 4 property
- A terminal with truecolor support
- Rust 1.74+, if you are building from source

## Contributing

`cargo run -- dash --demo` gets you a working dashboard with no Google account
attached. See [CONTRIBUTING.md](CONTRIBUTING.md) for the layout of the code and
what CI expects.

## License

[Apache License 2.0](LICENSE)


