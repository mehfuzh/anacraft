//! `craft burn` — the badge for somebody's own site.
//!
//! FeedBurner's chiclet is the shape of this, deliberately: a small served
//! image with a number in it, embedded on a page, carrying the name of the
//! thing that made it. That is also why this is the one part of the
//! subscription's surface that is free. A badge exists to be seen by people
//! who have never heard of anacraft, and charging for the privilege of putting
//! our name on somebody else's site would be charging the wrong direction.
//!
//! The number is how many other sites link to theirs — distinct referring
//! domains over the last thirty days, read from their own GA4 property.
//!
//! Thirty days rather than yesterday, and that is the whole design. The
//! complaint that followed FeedBurner around for a decade was a subscriber
//! count that jumped by hundreds overnight, and a number that visibly jitters
//! reads as broken whatever it says. A month-wide window moves slowly enough
//! that the badge is boring, which for a number on somebody's homepage is the
//! highest compliment available.
//!
//! Nothing about the account travels with the badge. The CLI counts locally
//! and publishes one integer, so the row behind the badge holds an answer and
//! never the credentials that produced it — the endpoint could not reach
//! anybody's Analytics if it tried.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

use crate::config::{self, Config};
use crate::ga::{DateRange, Ga, ReportRequest};
use crate::render::{bold, dim, paint};
use crate::theme::{self, glyph, ore};

/// The window the count is taken over. See the module note: this number is
/// chosen for how little it moves, not for how big it is.
const WINDOW_DAYS: u32 = 30;

/// How many source/medium rows to ask GA for.
///
/// The count is of distinct referring domains, so the request has to be deep
/// enough that the tail is not silently cut off — a site with two hundred
/// referrers would otherwise show whatever the cap was. Five hundred is past
/// anything a badge-wearing site is likely to have and still one request.
const SCAN_ROWS: u32 = 500;

/// The medium GA gives a visit that arrived by following a link.
const REFERRAL: &str = "referral";

/// What the badge says after the number, and what it says on the day there is
/// one of them.
///
/// The words are the unit, so they stay on the pill and the site's own name
/// does not: a badge reading `12 · anacraft.dev` never says what twelve is,
/// and it is read by people who have no idea what it is counting. The site's
/// name goes in the `alt` text, where it belongs and where it is worth
/// something.
///
/// Both forms travel to the service because the count moves and the label does
/// not: a badge that has said `12 sites` for a month should not say `1 sites`
/// the week somebody unlinks.
const DEFAULT_LABEL: &str = "sites";
const DEFAULT_LABEL_ONE: &str = "site";

/// The badge this machine minted, kept beside the other per-machine records.
///
/// The secret is the only thing that can change the number, so this is written
/// 0600 like the tokens and not into the shareable config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Badge {
    /// Public — this is what travels in the badge's URL.
    pub id: String,
    /// The write key. Everything else here is recoverable; this is not.
    pub secret: String,
    /// Which property the number was counted from, so a second property gets
    /// its own badge rather than overwriting this one.
    pub property: String,
    pub theme: String,
}

impl Badge {
    fn path() -> Result<PathBuf> {
        Ok(config::home()?.join("burn.json"))
    }

    fn load() -> Option<Badge> {
        Self::path()
            .ok()
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
    }

    fn save(&self) -> Result<()> {
        config::write_private(&Self::path()?, &serde_json::to_string_pretty(self)?)
    }

    /// Where the image lives. The project's own domain, because that is where
    /// the function is — a prettier host in front of it is a DNS change and
    /// nothing else, and the badge URL is the one thing here that cannot be
    /// changed afterwards without blanking every badge already embedded.
    pub fn url(&self, project: &str) -> String {
        format!("{project}/functions/v1/burn/{}.svg", self.id)
    }
}

/// Count the sites linking to this property.
///
/// One report, `sessionSourceMedium`, filtered down here rather than in the
/// request: GA writes the pair as "source / medium" in a single dimension, and
/// the referral half is the only one that means somebody followed a link.
/// Search engines, campaigns and direct arrivals are all traffic and none of
/// them are a site linking to you.
pub async fn count_linking_sites(ga: &Ga, property: &str) -> Result<usize> {
    let report = ga
        .report(
            property,
            ReportRequest::new(&["sessions"])
                .by(&["sessionSourceMedium"])
                .range(DateRange::last_days(WINDOW_DAYS))
                .top("sessions", SCAN_ROWS as i32),
        )
        .await
        .context("counting the sites that link here")?;

    let mut sites: Vec<String> = report
        .rows
        .iter()
        .filter_map(|row| referring_domain(row.dimension(0)))
        .collect();

    // GA can hand back the same domain more than once — "News.YCombinator.com"
    // and "news.ycombinator.com" are one site — so the dedupe is on the
    // lowered domain rather than on the string GA wrote.
    sites.sort();
    sites.dedup();
    Ok(sites.len())
}

/// The domain out of one `source / medium` cell, if it is a referral at all.
///
/// Split from the counting so the rule is testable without a network, because
/// the rule is the whole feature: everything else here is plumbing, and a
/// number that quietly counts Google as a site linking to you is a number
/// nobody should put on their homepage.
fn referring_domain(source_medium: &str) -> Option<String> {
    // From the right. GA writes the cell as `source / medium`, and a referrer
    // it kept a path on — `news.ycombinator.com/item / referral` — has slashes
    // of its own: splitting at the first one reads "item / referral" as the
    // medium and drops the row, which silently undercounts exactly the deep
    // links a badge is there to celebrate.
    let (source, medium) = source_medium.rsplit_once('/')?;
    if medium.trim().to_ascii_lowercase() != REFERRAL {
        return None;
    }
    let source = source.trim().to_ascii_lowercase();
    // GA's own placeholders are not domains.
    if source.is_empty() || source.starts_with('(') {
        return None;
    }
    // A referrer arrives as a host, sometimes with a path GA kept. Only the
    // host is the site, so `example.com/blog/post` and `example.com` are one.
    Some(
        source
            .split('/')
            .next()
            .unwrap_or(&source)
            .trim_start_matches("www.")
            .to_string(),
    )
}

/// `craft burn`.
pub async fn run(
    property: &str,
    theme: Option<String>,
    label: Option<String>,
    refresh: bool,
) -> Result<()> {
    let cfg = Config::load()?;
    // The site's own name, as its owner wrote it — for the alt text and for
    // the line this command prints, never for the pill. A property reached by
    // `--property` may not be in the config at all, which is not an error: the
    // badge simply has no name to put in its alt text.
    let site = cfg
        .find(property)
        .map(|p| p.display())
        .unwrap_or_else(|| format!("property {property}"));

    let Some((project, _)) = crate::license::project() else {
        bail!(
            "this build has no badge service configured — badges are served by \
             the anacraft project, and a self-built binary points at none"
        );
    };

    // The palette is resolved here rather than at serve time: the themes live
    // in this binary, so carrying the colors means a new one works the day it
    // ships instead of the day the endpoint is redeployed.
    let palette = match &theme {
        Some(name) => theme::THEMES
            .iter()
            .find(|p| p.name == name)
            .copied()
            .with_context(|| {
                format!(
                    "no theme called {name} — try {}",
                    theme::THEMES
                        .iter()
                        .map(|p| p.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
        None => theme::palette(),
    };

    let ga = Ga::new()?;
    // Somebody else's words are their own — a custom label is used as written
    // for both counts, since this has no business guessing how their language
    // makes a plural.
    let (label, label_one) = match label {
        Some(custom) => (custom.clone(), custom),
        None => (DEFAULT_LABEL.to_string(), DEFAULT_LABEL_ONE.to_string()),
    };
    // A property nobody has named reads as "property 397412345", which is an
    // id wearing a word. Not a site name, so not offered as one.
    let site = if site.starts_with("property ") {
        None
    } else {
        Some(site)
    };

    println!(
        "\n  {} counting the sites that link to {} over the last {WINDOW_DAYS} days…",
        glyph::PICKAXE,
        bold(site.as_deref().unwrap_or("this property")),
    );
    let count = count_linking_sites(&ga, property).await?;

    // An existing badge keeps its id: it is already in somebody's HTML by now,
    // and a badge that changes its own URL is a badge that goes blank.
    let existing = Badge::load().filter(|badge| badge.property == property);
    let badge = match existing {
        Some(badge) if refresh => badge,
        Some(badge) => Badge {
            theme: palette.name.to_string(),
            ..badge
        },
        None => Badge {
            id: crate::license::mint_token()[..16].to_string(),
            secret: crate::license::mint_token(),
            property: property.to_string(),
            theme: palette.name.to_string(),
        },
    };

    // Minting is idempotent and answers with whichever id the account already
    // had, so a second machine ends up pointing at the same badge rather than
    // starting a rival one.
    let account = crate::auth::Auth::account()?;
    let id = crate::license::rpc(
        "mint_badge",
        json!({
            "p_id": badge.id,
            "p_secret": badge.secret,
            "p_user_id": account.as_ref().map(|a| a.sub.as_str()),
            "p_property": property,
            "p_label": label,
            "p_label_one": label_one,
            "p_theme": badge.theme,
            "p_bg": hex(palette.bg),
            "p_fg": hex(palette.fg),
            "p_accent": hex(palette.accent),
            "p_shadow": hex(palette.shadow),
        }),
    )
    .await
    .context("minting the badge")?;

    let badge = Badge {
        id: id.trim().trim_matches('"').to_string(),
        ..badge
    };
    badge.save()?;

    crate::license::rpc(
        "publish_badge",
        json!({
            "p_id": badge.id,
            "p_secret": badge.secret,
            "p_count": count,
        }),
    )
    .await
    .context("publishing the count")?;

    let url = badge.url(&project);
    let plural = if count == 1 {
        "site links"
    } else {
        "sites link"
    };

    println!(
        "\n  {} {} {} to {}  ·  {} theme\n",
        paint(glyph::STAR, ore::gold()),
        bold(&paint(&count.to_string(), ore::gold())),
        plural,
        bold(site.as_deref().unwrap_or("this site")),
        dim(&badge.theme),
    );

    println!("  {}\n", dim("paste this where you want the badge:"));
    println!("{}\n", snippet(&url, count, site.as_deref()));

    println!(
        "  {}\n",
        dim(&format!(
            "the number is served, not baked in — re-run `craft burn --refresh` \
             to recount, and the badge on your page follows. {}",
            match count {
                0 => "nothing links here yet, so it reads 0 until something does.",
                _ => "it holds the last count until then.",
            }
        ))
    );

    Ok(())
}

/// The HTML to paste.
///
/// The link back is the entire business case for this feature being free, and
/// the `alt` text carries the number so the badge says something in a reader,
/// in a feed, and on a connection where the image never arrives.
fn snippet(url: &str, count: usize, site: Option<&str>) -> String {
    let plural = if count == 1 {
        "site links"
    } else {
        "sites link"
    };
    // Named where there is a name for it. "12 sites link to anacraft.dev" is
    // worth something in a reader, in a feed and to a search engine; "12 sites
    // link here" is what is left when the property was never given a name.
    let alt = match site {
        Some(site) => format!("{count} {plural} to {site}"),
        None => format!("{count} {plural} here"),
    };
    format!(
        "  <a href=\"https://anacraft.dev/burn.html\">\n    \
         <img src=\"{url}\" alt=\"{alt}\" height=\"20\">\n  </a>"
    )
}

fn hex(color: ratatui::style::Color) -> String {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        // Every shipped palette is truecolor; this is the safety net, and it
        // is the ink of the default theme rather than black so a badge built
        // from a hand-rolled palette still looks like one of ours.
        _ => "#111c18".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_followed_link_counts_as_a_site_linking_here() {
        // The rule the whole number rests on. Search, campaigns and direct
        // arrivals are traffic; none of them is a site linking to you, and a
        // badge that counted Google would be a badge that lies about the one
        // thing it claims to measure.
        assert_eq!(
            referring_domain("news.ycombinator.com / referral"),
            Some("news.ycombinator.com".into())
        );
        assert_eq!(referring_domain("google / organic"), None);
        assert_eq!(referring_domain("(direct) / (none)"), None);
        assert_eq!(referring_domain("newsletter / email"), None);
        // A cell with no medium at all is not a referral.
        assert_eq!(referring_domain("somewhere"), None);
    }

    #[test]
    fn one_site_is_counted_once_however_ga_spelled_it() {
        // GA hands back the host as it was written, so the same site arrives
        // in several spellings — and each extra spelling would be another
        // "site linking here" on somebody's homepage.
        let seen: Vec<Option<String>> = [
            "News.YCombinator.com / referral",
            "news.ycombinator.com / referral",
            "www.news.ycombinator.com / referral",
            "news.ycombinator.com/item / referral",
        ]
        .iter()
        .map(|cell| referring_domain(cell))
        .collect();

        for got in &seen {
            assert_eq!(got.as_deref(), Some("news.ycombinator.com"), "{seen:?}");
        }
    }

    #[test]
    fn the_snippet_links_back_and_says_the_number_without_the_image() {
        let html = snippet(
            "https://x.supabase.co/functions/v1/burn/abc.svg",
            12,
            Some("anacraft.dev"),
        );
        assert!(html.contains("https://anacraft.dev/burn.html"), "{html}");
        assert!(
            html.contains("alt=\"12 sites link to anacraft.dev\""),
            "{html}"
        );

        // One is one site, not one sites — the badge on a small site is the
        // one most likely to be read closely.
        let html = snippet("https://x/burn/abc.svg", 1, Some("example.com"));
        assert!(
            html.contains("alt=\"1 site links to example.com\""),
            "{html}"
        );
    }
}
