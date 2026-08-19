```
  ⛏  ANACRAFT
```

# anacraft

**Google Analytics, mined block by block.**

A Minecraft-themed TUI dashboard for Google Analytics 4. Real-time metrics, achievement toasts, page performance blocks, and realm maps — all rendered with ore colors and ore-textured bars right in your terminal.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.74+-orange.svg)](https://www.rust-lang.org/)

## Install

### Quick install (macOS / Linux)

```sh
curl -fsSL https://anacraft.dev/install.sh | bash
```

### From source

```sh
git clone https://github.com/smartloop-ai/anacraft.git
cd anacraft
cargo install --path .
```

### Manual download

Grab the latest binary from [Releases](https://github.com/smartloop-ai/anacraft/releases), extract, and put `anacraft` on your `PATH`.

## Quick start

```sh
# See the dashboard with synthetic data — no Google account needed
anacraft

# Connect a real GA4 property
anacraft login        # OAuth sign-in
anacraft props        # list visible properties
anacraft use 1234567  # set default property

# Launch the live TUI dashboard
anacraft
```

### One-shot reports

```sh
anacraft overview          # headline metrics + achievements
anacraft pages             # top pages ranked by views
anacraft realms            # traffic by country
anacraft live              # who is online right now
anacraft demo              # render overview from synthetic data
```

## Dashboard layout

The TUI dashboard has seven panels you can toggle with number keys:

| Key | Panel | What it shows |
|-----|-------|---------------|
| `1` | **WORLD** | Page performance blocks — green gem = rising, red = falling |
| `2` | **RIGHT NOW** | Live player count + event feed |
| `3` | **TOP CHUNKS** | Most-visited pages with view bars |
| `4` | **DAILY VILLAGERS** | User trend over the period |
| `5` | **REALMS MAP** | Traffic by country on a 10×5 grid map |
| `6` | **VITALS** | Users, sessions, views, conversions, bounce rate, avg session |
| `7` | **TOP REALMS** | Ranked countries with tier badges |

### Controls

| Key | Action |
|-----|--------|
| `1`–`7` | Toggle panel |
| `t` | Cycle theme (Cobblestone, Deepslate, Nether, Tokyo Night) |
| `r` | Force refresh |
| `?` / `h` | Help overlay |
| `q` / Esc | Quit |

## Themes

```sh
anacraft theme               # list palettes with color swatches
anacraft theme tokyo-night   # switch and persist
```

Saved to `~/.anacraft/config.json` — remembered across sessions.

## Configuration

| File | Purpose |
|------|---------|
| `~/.anacraft/credentials.json` | OAuth refresh token (created by `login`) |
| `~/.anacraft/config.json` | Default property, theme selection |

## Requirements

- Rust 1.74+ (for building from source)
- A Google Analytics 4 property
- A terminal with truecolor support (most modern terminals)

## License

[Apache License 2.0](LICENSE)

---

*Mined with ⛏ by [SmartLoop AI](https://github.com/smartloop-ai)*
