//! `craft slack` — install anacraft into a Slack workspace.
//!
//! What this replaces is six steps in a developer console. The first release
//! of `craft watch` sent people to api.slack.com to create an app, activate
//! incoming webhooks, choose a channel, copy a URL and paste it back into a
//! shell — to configure one destination.
//!
//! This is the handoff `craft login` already does. Slack's own install screen
//! carries the workspace and channel pickers, and the `incoming-webhook` scope
//! returns the URL in the OAuth response, so the channel is chosen where a
//! person expects to choose it and nothing is copied by hand.
//!
//! **No client secret is embedded.** Slack's PKCE support marks an app a
//! public client and drops the secret from the code exchange, which is the
//! only reason this can live in a binary anybody can download — the same
//! objection `SUBSCRIBE_URL` in `main.rs` records about Stripe keys. What
//! ships is a client id, which is public by design.
//!
//! The webhook URL that comes back is a credential: anyone holding it can post
//! into that channel. So it lands in `~/.anacraft/` at `0600` beside the OAuth
//! token, never in `config.toml`, which the README calls safe to commit to a
//! dotfile repo.

use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{self, encode, Pkce};
use crate::config;
use crate::render::{bold, dim, paint};
use crate::theme::{glyph, ore};

const AUTHORIZE_URL: &str = "https://slack.com/oauth/v2/authorize";
const ACCESS_URL: &str = "https://slack.com/api/oauth.v2.access";

/// One scope, and the narrowest one that does the job: permission to post to
/// the single channel the installer picks. Not `chat:write`, which would be
/// permission to post anywhere in the workspace as the app.
const SCOPE: &str = "incoming-webhook";

/// The app's client id, baked in at build time like the Google one. Public by
/// design — see the module note.
fn client_id() -> Option<&'static str> {
    non_empty(option_env!("ANACRAFT_SLACK_CLIENT_ID"))
}

/// Where Slack sends the code.
///
/// Slack refuses a loopback redirect for a *distributed* app: every URL on one
/// has to be HTTPS. So a published build points at a relay that bounces the
/// code back to the port this process is listening on, carried in `state`.
///
/// Unset means no relay, and the flow redirects straight to the loopback port.
/// That is what an undistributed app allows, which is how this gets tested
/// against one workspace before public distribution exists.
fn relay_url() -> Option<&'static str> {
    non_empty(option_env!("ANACRAFT_SLACK_REDIRECT"))
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.filter(|v| !v.trim().is_empty())
}

// ----------------------------------------------------------------- record ---

/// What an install leaves behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Install {
    /// The credential. Held here and nowhere else.
    pub webhook_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub installed_at: DateTime<Utc>,
}

impl Install {
    fn path() -> Result<PathBuf> {
        Ok(config::home()?.join("slack.json"))
    }

    /// The saved install, or `None` if there is not one.
    ///
    /// An unreadable file reads as absent rather than as an error: the only
    /// thing downstream does with this is decide whether to post, and a
    /// corrupt record should send somebody to `craft slack --install`, not
    /// stop their watch from running.
    pub fn load() -> Option<Install> {
        let path = Self::path().ok()?;
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn save(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        config::write_private(&Self::path()?, &raw)
    }

    fn clear() -> Result<()> {
        let path = Self::path()?;
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        Ok(())
    }

    /// How to name the destination out loud. Slack gives the channel back with
    /// its `#`, but not always, and not always at all.
    pub fn destination(&self) -> String {
        match (&self.channel, &self.team) {
            (Some(channel), Some(team)) => format!("{channel} in {team}"),
            (Some(channel), None) => channel.clone(),
            (None, Some(team)) => team.clone(),
            (None, None) => "your Slack workspace".to_string(),
        }
    }
}

// ------------------------------------------------------------- the exchange --

/// Slack answers a failed call with HTTP 200 and `ok: false`, so the status
/// line is not the check — this is.
#[derive(Deserialize)]
struct AccessResponse {
    ok: bool,
    error: Option<String>,
    incoming_webhook: Option<WebhookField>,
    team: Option<TeamField>,
}

#[derive(Deserialize)]
struct WebhookField {
    url: String,
    channel: Option<String>,
}

#[derive(Deserialize)]
struct TeamField {
    name: Option<String>,
}

/// Trade the authorization code for the webhook Slack minted.
///
/// The endpoint is a parameter so this can be pointed at a local server in a
/// test. Slack's failure mode is the reason it is worth testing at all: a
/// refused exchange comes back as HTTP 200 with `ok: false`, so code that
/// trusts the status line reads a rejection as a success and saves a record
/// with no webhook in it.
async fn exchange(
    access_url: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Install> {
    // No client_secret: PKCE is what makes this exchange safe from a public
    // client, and what keeps a secret out of a downloadable binary.
    let response = reqwest::Client::new()
        .post(access_url)
        .form(&[
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .context("could not reach Slack to finish the install")?;

    let body: AccessResponse = response
        .json()
        .await
        .context("unexpected response shape from Slack")?;

    if !body.ok {
        bail!(
            "Slack refused the install: {}",
            body.error.unwrap_or_else(|| "no reason given".into())
        );
    }

    let hook = body.incoming_webhook.context(
        "Slack approved the install but sent no webhook — \
         was the incoming-webhook scope requested?",
    )?;

    Ok(Install {
        webhook_url: hook.url,
        channel: hook.channel,
        team: body.team.and_then(|t| t.name),
        installed_at: Utc::now(),
    })
}

// ------------------------------------------------------------------ entry ---

/// Run the install: PKCE, browser handoff, code exchange, save.
pub async fn install() -> Result<()> {
    let client_id = client_id().context(
        "this build has no Slack app configured.\n     \
         Set ANACRAFT_SLACK_CLIENT_ID at build time, or use \
         `craft watch --webhook <url>` with a webhook you made yourself at \
         https://api.slack.com/apps",
    )?;

    let Pkce {
        verifier,
        challenge,
    } = auth::pkce();

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("could not open a local port for the Slack redirect")?;
    let port = listener.local_addr()?.port();

    // The port has to survive the round trip. A relay has nowhere to keep it,
    // so it rides inside `state` — which is returned verbatim and checked in
    // full, so carrying a second value in it costs none of its purpose.
    let nonce = auth::nonce(24);
    let (redirect_uri, state) = match relay_url() {
        Some(relay) => (relay.to_string(), format!("{nonce}.{port}")),
        None => (format!("http://127.0.0.1:{port}"), nonce),
    };

    let url = format!(
        "{AUTHORIZE_URL}?client_id={}&scope={}&redirect_uri={}&state={}\
         &code_challenge={}&code_challenge_method=S256",
        encode(client_id),
        encode(SCOPE),
        encode(&redirect_uri),
        encode(&state),
        encode(&challenge),
    );

    println!(
        "\n  {} opening your browser to install anacraft in Slack…",
        paint(glyph::PICKAXE, ore::gold())
    );
    println!("  {}\n", dim("pick the workspace and the channel there"));
    println!("  {}\n\n  {url}\n", dim("if it doesn't open, paste this:"));
    let _ = open::that(&url);

    let code = auth::wait_for_code(
        &listener,
        &state,
        (
            "Installed",
            "anacraft can post to the channel you picked. \
             You can close this tab and return to the terminal.",
        ),
    )?;

    let record = exchange(ACCESS_URL, client_id, &code, &verifier, &redirect_uri).await?;
    record.save()?;

    println!(
        "  {} {}  ·  {}\n",
        paint(glyph::STAR, ore::gold()),
        bold(&paint("installed", ore::emerald())),
        dim(&format!("alerts go to {}", record.destination()))
    );
    println!(
        "  {}\n",
        dim("`craft watch` posts there now — no --webhook needed. \
             `craft slack --test` sends one to check.")
    );
    Ok(())
}

/// Forget the install. Slack still holds the app until it is removed there.
pub fn uninstall() -> Result<()> {
    match Install::load() {
        None => {
            println!("\n  {}\n", dim("no Slack install to remove"));
            Ok(())
        }
        Some(record) => {
            Install::clear()?;
            println!(
                "\n  {} {}  ·  {}\n",
                paint(glyph::PICKAXE, ore::redstone()),
                bold("removed"),
                dim(&format!("was posting to {}", record.destination()))
            );
            // The URL is gone from this machine, which is all this command can
            // do. Saying so is the difference between a user believing the app
            // is revoked and knowing it is not.
            println!(
                "  {}\n",
                dim("the app is still installed in Slack — remove it there to \
                     revoke the webhook itself")
            );
            Ok(())
        }
    }
}

/// Say where alerts go, if anywhere.
pub fn status() -> Result<()> {
    match Install::load() {
        Some(record) => {
            println!(
                "\n  {} {}  ·  {}\n",
                paint(glyph::STAR, ore::gold()),
                bold(&paint("slack", ore::emerald())),
                dim(&format!(
                    "alerts go to {} · installed {}",
                    record.destination(),
                    record.installed_at.format("%-d %b %Y")
                ))
            );
        }
        None => {
            println!(
                "\n  {} {}\n\n  {}\n",
                paint(glyph::PICKAXE, ore::stone()),
                bold("no Slack destination yet"),
                dim("run `craft slack --install` to pick a channel")
            );
        }
    }
    Ok(())
}

/// Post one message, to prove the install works before an alert depends on it.
pub async fn test() -> Result<()> {
    let record = Install::load()
        .context("no Slack install on this machine — run `craft slack --install` first")?;

    let payload = json!({
        // Same reason `craft watch` carries one: mobile notifications use the
        // top-level text and nothing else, and a test message that arrives on
        // a phone as a blank push has disproved the thing it was sent to prove.
        "text": "⛏ anacraft is connected — this is where your alerts will land.",
        "blocks": [
            {
                "type": "header",
                "text": { "type": "plain_text", "text": "⛏ anacraft is connected" },
            },
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": "This is where your alerts will land. \
                             `craft watch` posts here when a metric moves \
                             further than it usually does — or goes silent.",
                },
            },
        ]
    });

    crate::watch::post(&record.webhook_url, &payload).await?;
    println!(
        "\n  {} {}  ·  {}\n",
        paint(glyph::STAR, ore::gold()),
        bold(&paint("sent", ore::emerald())),
        dim(&format!("check {}", record.destination()))
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(channel: Option<&str>, team: Option<&str>) -> Install {
        Install {
            webhook_url: "https://hooks.slack.com/services/T/B/x".into(),
            channel: channel.map(str::to_string),
            team: team.map(str::to_string),
            installed_at: Utc::now(),
        }
    }

    #[test]
    fn a_destination_reads_as_a_place_whatever_slack_sent_back() {
        assert_eq!(
            record(Some("#alerts"), Some("Acme")).destination(),
            "#alerts in Acme"
        );
        assert_eq!(record(Some("#alerts"), None).destination(), "#alerts");
        assert_eq!(record(None, Some("Acme")).destination(), "Acme");
        // Slack sent neither. Still a sentence.
        assert_eq!(record(None, None).destination(), "your Slack workspace");
    }

    #[test]
    fn the_record_round_trips_and_keeps_the_url() {
        let before = record(Some("#alerts"), Some("Acme"));
        let raw = serde_json::to_string(&before).unwrap();
        let after: Install = serde_json::from_str(&raw).unwrap();

        assert_eq!(after.webhook_url, before.webhook_url);
        assert_eq!(after.channel, before.channel);
    }

    #[test]
    fn a_record_slack_answered_thinly_still_loads() {
        // channel and team are both optional on the way in, so a response
        // that carried neither must not make the install unreadable later.
        let after: Install = serde_json::from_str(
            r#"{"webhook_url":"https://hooks.slack.com/services/T/B/x",
                "installed_at":"2026-09-04T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(after.destination(), "your Slack workspace");
    }

    /// A one-shot HTTP server that answers with `body`, so the exchange can be
    /// exercised without api.slack.com. Returns the URL to point it at.
    fn fake_slack(body: &'static str) -> String {
        use std::io::{Read, Write};
        use std::net::{Ipv4Addr, TcpListener};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain enough of the request that the client's write finishes
                // before we answer; the body itself is not what is under test.
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                // Slack answers everything with 200, which is the point.
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        format!("http://127.0.0.1:{port}/api/oauth.v2.access")
    }

    #[tokio::test]
    async fn an_approved_exchange_yields_the_webhook_slack_minted() {
        let url = fake_slack(
            r##"{"ok":true,"team":{"name":"Acme"},"incoming_webhook":{"channel":"#alerts","url":"https://hooks.slack.com/services/T/B/x"}}"##,
        );

        let record = exchange(&url, "cid", "code", "verifier", "http://127.0.0.1:1")
            .await
            .expect("approved");

        assert_eq!(record.webhook_url, "https://hooks.slack.com/services/T/B/x");
        assert_eq!(record.destination(), "#alerts in Acme");
    }

    #[tokio::test]
    async fn a_refusal_is_an_error_even_though_slack_said_200() {
        let url = fake_slack(r#"{"ok":false,"error":"invalid_code"}"#);

        let err = exchange(&url, "cid", "bad", "verifier", "http://127.0.0.1:1")
            .await
            .expect_err("a rejection must not read as an install")
            .to_string();

        assert!(err.contains("invalid_code"), "unhelpful: {err}");
    }

    #[tokio::test]
    async fn an_approval_with_no_webhook_says_which_scope_is_missing() {
        // What a install approved without `incoming-webhook` looks like: ok,
        // but nothing to post to. Saving that would leave a record whose
        // whole purpose is a URL it does not have.
        let url = fake_slack(r#"{"ok":true,"team":{"name":"Acme"}}"#);

        let err = exchange(&url, "cid", "code", "verifier", "http://127.0.0.1:1")
            .await
            .expect_err("no webhook is not an install")
            .to_string();

        assert!(err.contains("incoming-webhook"), "unhelpful: {err}");
    }

    #[test]
    fn a_failed_call_is_read_off_the_body_not_the_status() {
        // Slack answers this with HTTP 200.
        let body: AccessResponse =
            serde_json::from_str(r#"{"ok":false,"error":"invalid_code"}"#).unwrap();
        assert!(!body.ok);
        assert_eq!(body.error.as_deref(), Some("invalid_code"));
    }

    #[test]
    fn the_webhook_and_channel_come_out_of_the_oauth_response() {
        // `r##` rather than `r#`: the channel name contains `"#`, which ends
        // a single-hash raw string right in the middle of the fixture.
        let body: AccessResponse = serde_json::from_str(
            r##"{"ok":true,
                 "team":{"id":"T1","name":"Acme"},
                 "incoming_webhook":{"channel":"#alerts",
                                     "url":"https://hooks.slack.com/services/T/B/x"}}"##,
        )
        .unwrap();

        assert!(body.ok);
        let hook = body.incoming_webhook.unwrap();
        assert_eq!(hook.channel.as_deref(), Some("#alerts"));
        assert!(hook.url.starts_with("https://hooks.slack.com/"));
        assert_eq!(body.team.and_then(|t| t.name).as_deref(), Some("Acme"));
    }
}
