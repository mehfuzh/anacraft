//! `craft configure <domain>` — the setup guide, run as a command.
//!
//! `docs/setup-ga4.html` is the most-read page on the site, and steps 01–03 of
//! it are the reason: create a property, add a web data stream, copy the
//! measurement id into a snippet. None of that is a decision. It is four
//! screens of console navigation standing between somebody and their first
//! number, and every one of those screens is an Admin API call.
//!
//! So this does the three steps and prints the tag with the real id already in
//! it. What it deliberately does not do is guess: it will reuse a property
//! that already measures the domain rather than quietly create a second one,
//! and it stops and asks when the account is ambiguous.
//!
//! This is the only command in anacraft that writes anything to a Google
//! account, and it asks for the permission to do so itself — see
//! `auth::ensure_scope` and `docs/oauth-scopes.md`.

use anyhow::{bail, Context, Result};

use crate::auth::{Tokens, SCOPE_EDIT};
use crate::config::Config;
use crate::ga::{Account, Ga, Property, WebStream};
use crate::render::{bold, dim, paint, panel_bottom, panel_top};
use crate::theme::{glyph, ore};

/// How many properties to search for an existing stream before giving up on
/// the reuse check. An account with more properties than this is a reseller's,
/// and the scan is one API call each.
const SCAN_LIMIT: usize = 40;

pub struct Options {
    /// GA4 account to create under, when the login can see more than one.
    pub account: Option<String>,
    /// IANA reporting time zone. Detected from the machine when absent.
    pub timezone: Option<String>,
    /// ISO 4217 reporting currency.
    pub currency: String,
}

pub async fn run(domain: &str, opts: Options) -> Result<()> {
    let host = host_of(domain)?;
    let uri = format!("https://{host}");

    let ga = Ga::new()?;
    let why =
        format!("setting up {host} needs permission to add a property to your Analytics account");

    // A machine that has never signed in needs one browser trip, not two, so
    // the cold start asks for both at once. Everything above this line is
    // local, so somebody who declines has changed nothing.
    if Tokens::load()?.is_none() {
        ga.auth().ensure_scope(SCOPE_EDIT, &why).await?;
    }

    println!();
    let property = match find_existing(&ga, &host).await? {
        Some(Existing::Measured(property, stream)) => {
            // Running this twice is the normal way to get the tag back.
            // Creating a second property for the same site would split the
            // numbers in two, and nothing would say so until a week of data
            // had gone to the wrong one.
            save(&property.id, &host)?;
            println!(
                "  {} {} is already measured by {} {}",
                paint("✓", ore::emerald()),
                bold(&host),
                bold(&paint(&property.name, ore::diamond())),
                dim(&format!("({})", property.id)),
            );
            println!("  {}\n", dim("nothing was created — here is its tag again"));
            print_tag(&stream.measurement_id);
            print_next(&host);
            return Ok(());
        }

        // Reaching either arm below means something is about to be created,
        // and that is the first moment anything is. Somebody who only wanted
        // their tag back was served above, entirely within read-only access,
        // and was never shown a consent screen.
        Some(Existing::Unfinished(property)) => {
            ga.auth().ensure_scope(SCOPE_EDIT, &why).await?;
            println!(
                "  {} finishing {} {}",
                glyph::PICKAXE,
                bold(&paint(&host, ore::diamond())),
                dim(&format!(
                    "({}, created earlier but never given a stream)",
                    property.id
                )),
            );
            property
        }

        None => {
            ga.auth().ensure_scope(SCOPE_EDIT, &why).await?;
            let account = pick_account(&ga, opts.account.as_deref()).await?;
            let timezone = match opts.timezone {
                Some(tz) => tz,
                None => local_timezone().unwrap_or_else(|| {
                    println!(
                        "  {}",
                        dim(
                            "could not read this machine's time zone; reporting in UTC — \
                             re-run with --timezone to change it"
                        )
                    );
                    "UTC".to_string()
                }),
            };

            println!(
                "  {} creating a property for {} in {}",
                glyph::PICKAXE,
                bold(&host),
                dim(&account.name),
            );
            let property = ga
                .create_property(&account, &host, &timezone, &opts.currency)
                .await
                .with_context(|| format!("creating a property for {host}"))?;
            println!(
                "  {} property {} {}",
                paint("✓", ore::emerald()),
                bold(&paint(&host, ore::diamond())),
                dim(&format!("({}, reporting in {timezone})", property.id)),
            );
            property
        }
    };

    // The property exists from here on. A failure below leaves it behind
    // rather than rolling back — an empty property costs nothing, and deleting
    // one on somebody's behalf because a second call failed is the more
    // surprising outcome. Re-running is safe: the next run recognises that
    // property as unfinished and adds the stream to it.
    let stream = ga
        .create_web_stream(&property.id, &host, &uri)
        .await
        .with_context(|| {
            format!(
                "property {} was created, but adding its web stream failed — \
                 re-run this command to finish it",
                property.id
            )
        })?;

    save(&property.id, &host)?;

    println!(
        "  {} web stream {} {}\n",
        paint("✓", ore::emerald()),
        bold(&stream.measurement_id),
        dim(&format!("({uri})")),
    );

    print_tag(&stream.measurement_id);
    print_next(&host);
    Ok(())
}

// ------------------------------------------------------------------ lookup ---

/// What the account already has for this domain.
enum Existing {
    /// A property with a web stream pointing at the domain. Nothing to do but
    /// print its tag.
    Measured(Property, WebStream),
    /// A property named after the domain with no web stream on it — what a run
    /// that created the property and then failed leaves behind. Finishing it is
    /// what the failure told the user to do, and doing so must not make a
    /// second property.
    Unfinished(Property),
}

/// What already exists for `host`, if anything.
///
/// Properties named after the domain are checked first, because that is what
/// this command names them, so the common re-run costs one extra call rather
/// than one per property.
async fn find_existing(ga: &Ga, host: &str) -> Result<Option<Existing>> {
    let mut properties = ga.properties().await?;
    properties.sort_by_key(|p| p.name.to_lowercase() != host);

    let total = properties.len();
    let mut unfinished = None;

    for property in properties.into_iter().take(SCAN_LIMIT) {
        // A property this login cannot read streams on is not a match; it is
        // also not a reason to fail, so skip it.
        let Ok(streams) = ga.web_streams(&property.id).await else {
            continue;
        };

        if let Some(stream) = streams
            .iter()
            .find(|s| host_of(&s.default_uri).is_ok_and(|h| h == host))
        {
            // A live stream beats an empty property, wherever each was found,
            // so this returns immediately and the fallback below never wins
            // over a real match.
            return Ok(Some(Existing::Measured(
                property,
                WebStream {
                    measurement_id: stream.measurement_id.clone(),
                    default_uri: stream.default_uri.clone(),
                },
            )));
        }

        // Only an exact name match, and only the first: adding a stream to
        // some unrelated property because it happened to be empty would be a
        // change nobody asked for.
        if unfinished.is_none() && streams.is_empty() && property.name.to_lowercase() == host {
            unfinished = Some(property);
        }
    }

    if unfinished.is_none() && total > SCAN_LIMIT {
        println!(
            "  {}",
            dim(&format!(
                "checked the first {SCAN_LIMIT} of {total} properties for an existing \
                 stream on this domain"
            ))
        );
    }
    Ok(unfinished.map(Existing::Unfinished))
}

/// Which account to create in: the named one, the only one, or a question.
async fn pick_account(ga: &Ga, wanted: Option<&str>) -> Result<Account> {
    let accounts = ga.accounts().await?;

    if accounts.is_empty() {
        // Creating the *account* is the one step that cannot move in here.
        // It requires accepting Google's terms, which is a person's decision
        // to make in Google's own words — and the scope that would let a
        // client do it, `analytics.provision`, is far wider than anything
        // else this command needs.
        bail!(
            "this Google account has no Analytics account to create a property in.\n  \
             Create one at https://analytics.google.com/analytics/web/#/provision \
             (it takes a minute, and accepting the terms has to happen there),\n  \
             then run this again."
        );
    }

    if let Some(wanted) = wanted {
        let needle = wanted.trim().trim_start_matches("accounts/");
        return accounts
            .into_iter()
            .find(|a| a.id == needle || a.name.eq_ignore_ascii_case(needle))
            .with_context(|| format!("no Analytics account matching {wanted}"));
    }

    if accounts.len() == 1 {
        return Ok(accounts.into_iter().next().expect("length checked"));
    }

    // Picking one for somebody would put the property — and its data retention,
    // and who can see it — in a place they did not choose.
    let list = accounts
        .iter()
        .map(|a| format!("\n    {}  {}", a.id, a.name))
        .collect::<String>();
    bail!(
        "this login can see {} Analytics accounts, so pick the one to create in:{list}\n\n  \
         then: craft configure <domain> --account <id>",
        accounts.len()
    );
}

// ------------------------------------------------------------------ output ---

/// The gtag.js snippet, with the id already in both places it belongs.
///
/// Both places is the point. The single most common broken install is the id
/// pasted into the `src` and left as a placeholder in the `config` call, or the
/// reverse — the tag loads, nothing is recorded, and the console says only that
/// no data has been received.
///
/// The id is painted, and nothing else is: ANSI codes are not part of the text
/// a terminal copies, so this stays paste-safe, and `paint` drops them anyway
/// the moment stdout is not a TTY.
fn tag(measurement_id: &str) -> String {
    let id = paint(measurement_id, ore::gold());
    format!(
        "  {}\n  \
         <script async src=\"https://www.googletagmanager.com/gtag/js?id={id}\"></script>\n  \
         <script>\n    \
         window.dataLayer = window.dataLayer || [];\n    \
         function gtag(){{dataLayer.push(arguments);}}\n    \
         gtag('js', new Date());\n    \
         gtag('config', '{id}');\n  \
         </script>",
        dim("<!-- Google tag (gtag.js) -->"),
    )
}

fn print_tag(measurement_id: &str) {
    println!("{}\n", panel_top("PASTE THIS IN <HEAD>"));
    println!("{}\n", tag(measurement_id));
    println!("{}\n", panel_bottom());
    println!(
        "  {}",
        dim("on every page, and only once — a second GA4 tag doubles every number")
    );
}

fn print_next(host: &str) {
    println!(
        "  {} then {} to watch {host} arrive\n",
        dim("↳"),
        bold("craft live"),
    );
}

/// Remember the property, and open on it from now on.
fn save(id: &str, host: &str) -> Result<()> {
    let mut cfg = Config::load()?;
    cfg.upsert(id, Some(host.to_string()));
    cfg.save()
}

// ------------------------------------------------------------------ inputs ---

/// The bare host out of whatever somebody typed.
///
/// People paste what they have: a bare domain, the address bar, a link with a
/// path on it. All three name the same site, and refusing two of them would be
/// pedantry. What is refused is input that names something *else* — a path, a
/// port, an email address — because a data stream's default URI is an origin,
/// and silently discarding half of what was typed is how the wrong site gets
/// measured.
pub fn host_of(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('.');
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    // A trailing slash, or a path/query/fragment, all end the host.
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();

    if host.is_empty() {
        bail!("which domain? e.g. craft configure example.com");
    }
    if host.contains('@') {
        bail!("{host} looks like an email address, not a domain");
    }
    if host.contains(':') {
        bail!("{host} names a port; Analytics measures an origin — drop the port");
    }
    if !host.contains('.') {
        bail!("{host} is not a domain — it needs a dot, e.g. example.com");
    }
    if host
        .chars()
        .any(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '.')
    {
        bail!("{host} is not a domain anacraft can read — pass it as example.com");
    }
    Ok(host)
}

/// This machine's IANA time zone, which is what a GA4 property wants.
///
/// `TZ` first, then the `/etc/localtime` symlink that both macOS and Linux
/// keep. Neither is guaranteed, so the caller handles `None` rather than this
/// inventing a plausible-looking zone — a property reporting in the wrong day
/// boundary is a subtle thing to discover later.
fn local_timezone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim().trim_start_matches(':').to_string();
        if is_iana(&tz) {
            return Some(tz);
        }
    }
    let link = std::fs::read_link("/etc/localtime").ok()?;
    let path = link.to_string_lossy();
    let tz = path.split_once("zoneinfo/").map(|(_, tz)| tz)?.to_string();
    is_iana(&tz).then_some(tz)
}

/// Shaped like `Area/City`, which is all that can be checked without shipping
/// the tz database. Google rejects anything it does not recognise, and says so.
fn is_iana(tz: &str) -> bool {
    let mut parts = tz.split('/');
    let (Some(area), Some(city)) = (parts.next(), parts.next()) else {
        return false;
    };
    !area.is_empty()
        && !city.is_empty()
        && tz
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_host_out_of_whatever_was_typed() {
        for raw in [
            "example.com",
            "  example.com  ",
            "https://example.com",
            "http://example.com/",
            "https://example.com/pricing?ref=x#top",
            "EXAMPLE.COM",
            "example.com.",
        ] {
            assert_eq!(host_of(raw).unwrap(), "example.com", "input: {raw}");
        }
    }

    #[test]
    fn a_subdomain_is_its_own_site() {
        // www.example.com and example.com are different origins, and which one
        // the tag ends up on is the user's call, not ours to normalise away.
        assert_eq!(host_of("www.example.com").unwrap(), "www.example.com");
        assert_eq!(host_of("blog.example.co.uk").unwrap(), "blog.example.co.uk");
    }

    #[test]
    fn refuses_input_that_names_something_other_than_a_site() {
        for raw in ["", "   ", "localhost", "me@example.com", "example.com:3000"] {
            assert!(host_of(raw).is_err(), "accepted: {raw:?}");
        }
        // A path is dropped, but the host in front of it still has to be one.
        assert!(host_of("https:///pricing").is_err());
    }

    #[test]
    fn the_measurement_id_lands_in_both_places_the_tag_needs_it() {
        // The classic broken install is one of the two filled in and the other
        // left as a placeholder. The tag loads, nothing is recorded, and GA4
        // reports only that no data has arrived.
        let snippet = tag("G-1A2BCD345E");
        assert_eq!(
            snippet.matches("G-1A2BCD345E").count(),
            2,
            "the id belongs in the script src and the config call:\n{snippet}"
        );
        assert!(snippet.contains(
            "<script async src=\"https://www.googletagmanager.com/gtag/js?id=G-1A2BCD345E\">"
        ));
        assert!(snippet.contains("gtag('config', 'G-1A2BCD345E');"));
        // No placeholder survived into what somebody is about to paste.
        assert!(!snippet.contains("G-XXXXXXXXXX"));
    }

    #[test]
    fn the_tag_is_the_one_the_setup_guide_documents() {
        // docs/setup-ga4.html step 03 prints this snippet for people doing it
        // by hand. The two must not drift: a difference here would read as one
        // of them being wrong.
        let snippet = tag("G-1A2BCD345E");
        for line in [
            "<!-- Google tag (gtag.js) -->",
            "window.dataLayer = window.dataLayer || [];",
            "function gtag(){dataLayer.push(arguments);}",
            "gtag('js', new Date());",
            "</script>",
        ] {
            assert!(snippet.contains(line), "missing {line:?}:\n{snippet}");
        }
    }

    #[test]
    fn a_time_zone_has_to_look_like_one() {
        assert!(is_iana("America/Los_Angeles"));
        assert!(is_iana("Asia/Dhaka"));
        assert!(is_iana("America/Argentina/Buenos_Aires"));
        for bad in ["UTC", "", "/", "PST8PDT", "Europe/", "/London", "a b/c"] {
            assert!(!is_iana(bad), "accepted: {bad:?}");
        }
    }
}
