//! OAuth 2.0 installed-application flow against Google, hand-rolled so the
//! whole login experience (including the browser success page) stays on-theme
//! and we carry no extra dependency surface.
//!
//! Google treats the client secret of a *Desktop* OAuth client as
//! non-confidential, which is what makes it safe to bake into a shipped binary.
//! We still use PKCE, which is what actually protects the exchange.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

/// Analytics, plus the two non-sensitive OpenID scopes.
///
/// The identity scopes are not there to read anything about the person: they
/// are how a subscription survives a new laptop. Stripe's webhook writes the
/// Google account id against the payment, and a fresh machine that signs into
/// the same account gets its subscription back without anybody copying a token
/// around. `openid` and `email` are non-sensitive, so unlike a wider Analytics
/// scope they add nothing to the consent review — see the test below.
const SCOPE: &str = "openid email https://www.googleapis.com/auth/analytics.readonly";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";

/// Baked in at build time by the release pipeline:
///   ANACRAFT_OAUTH_CLIENT_ID=... ANACRAFT_OAUTH_CLIENT_SECRET=... cargo build --release
const BUILTIN_ID: Option<&str> = option_env!("ANACRAFT_OAUTH_CLIENT_ID");
const BUILTIN_SECRET: Option<&str> = option_env!("ANACRAFT_OAUTH_CLIENT_SECRET");

#[derive(Clone, Serialize, Deserialize)]
pub struct ClientCreds {
    pub client_id: String,
    pub client_secret: String,
}

impl ClientCreds {
    /// Env vars win (so contributors can point at their own project), then a
    /// local `client.json`, then whatever was compiled in.
    pub fn load() -> Result<ClientCreds> {
        if let (Ok(id), Ok(secret)) = (
            std::env::var("ANACRAFT_OAUTH_CLIENT_ID"),
            std::env::var("ANACRAFT_OAUTH_CLIENT_SECRET"),
        ) {
            if !id.trim().is_empty() {
                return Ok(ClientCreds {
                    client_id: id,
                    client_secret: secret,
                });
            }
        }

        let path = crate::config::home()?.join("client.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            return serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()));
        }

        match (BUILTIN_ID, BUILTIN_SECRET) {
            (Some(id), Some(secret)) if !id.is_empty() => Ok(ClientCreds {
                client_id: id.to_string(),
                client_secret: secret.to_string(),
            }),
            _ => bail!(
                "no OAuth client configured.\n\n\
                 This build has no client baked in. Create a *Desktop app* OAuth client at\n\
                 https://console.cloud.google.com/apis/credentials, enable the Google Analytics\n\
                 Data API + Admin API, then either:\n\n  \
                 export ANACRAFT_OAUTH_CLIENT_ID=... ANACRAFT_OAUTH_CLIENT_SECRET=...\n\n\
                 or write ~/.anacraft/client.json:\n  \
                 {{\"client_id\": \"...\", \"client_secret\": \"...\"}}"
            ),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    /// Absent on credentials written before identity was asked for. Those still
    /// work for every report; only carrying a subscription to another machine
    /// needs a `craft login` to fill this in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
}

impl Tokens {
    fn path() -> Result<std::path::PathBuf> {
        Ok(crate::config::home()?.join("token.json"))
    }

    pub fn load() -> Result<Option<Tokens>> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&raw).ok())
    }

    pub fn save(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        crate::config::write_private(&Self::path()?, &raw)
    }

    pub fn clear() -> Result<()> {
        let path = Self::path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Refresh a minute early so a long report can't expire mid-flight.
    fn is_stale(&self) -> bool {
        Utc::now() + Duration::seconds(60) >= self.expires_at
    }
}

/// Raw shape of Google's token endpoint response.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    /// Present whenever `openid` was granted. Carries the account id, so no
    /// separate userinfo round trip is needed.
    #[serde(default)]
    id_token: Option<String>,
}

/// Who Google says is signed in.
///
/// `sub` is Google's stable, opaque id for the account — it survives an email
/// change, which is exactly what a subscription needs to be keyed on. The email
/// rides along only so a support question ("which account did I pay with?") has
/// an answer a human recognises.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub sub: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Read the account out of an id token.
///
/// The token came straight from Google's token endpoint over TLS, so the
/// signature is not re-checked here: there is no untrusted hop to protect
/// against, and a JWT library for one field would be a dependency for nothing.
/// A malformed token is simply no identity, never an error that blocks a login.
fn account_from_id_token(id_token: &str) -> Option<Account> {
    let payload = id_token.split('.').nth(1)?;
    let raw = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Account>(&raw)
        .ok()
        .filter(|a| !a.sub.is_empty())
}

pub struct Auth {
    http: reqwest::Client,
    creds: ClientCreds,
}

impl Auth {
    pub fn new(http: reqwest::Client) -> Result<Auth> {
        Ok(Auth {
            http,
            creds: ClientCreds::load()?,
        })
    }

    /// A valid bearer token, refreshing transparently when needed.
    pub async fn access_token(&self) -> Result<String> {
        let mut tokens = Tokens::load()?.context("not logged in — run `craft login`")?;

        if tokens.is_stale() {
            tokens = self.refresh(&tokens.refresh_token).await?;
            tokens.save()?;
        }
        Ok(tokens.access_token)
    }

    /// The signed-in Google account, if the stored credentials carry one.
    pub fn account() -> Result<Option<Account>> {
        Ok(Tokens::load()?.and_then(|t| t.account))
    }

    async fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        let res = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", self.creds.client_id.as_str()),
                ("client_secret", self.creds.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .context("contacting Google token endpoint")?;

        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            // A revoked or expired refresh token is unrecoverable; make the
            // fix obvious instead of surfacing raw JSON.
            bail!("session expired — run `craft login` again\n  ({body})");
        }

        let body: TokenResponse = res.json().await?;
        Ok(Tokens {
            access_token: body.access_token,
            // Refresh responses omit refresh_token; keep the one we have.
            refresh_token: body
                .refresh_token
                .unwrap_or_else(|| refresh_token.to_string()),
            expires_at: Utc::now() + Duration::seconds(body.expires_in),
            // Google re-issues the id token on refresh only when `openid` was
            // granted at consent, so credentials from before the identity
            // scopes stay identity-less until the next `craft login`. That
            // costs them nothing but the cross-machine lookup — which is why
            // the existing account, if any, is kept rather than cleared.
            account: body
                .id_token
                .as_deref()
                .and_then(account_from_id_token)
                .or_else(|| Tokens::load().ok().flatten().and_then(|t| t.account)),
        })
    }

    /// Full interactive login: PKCE + loopback redirect + browser handoff.
    pub async fn login(&self) -> Result<()> {
        let verifier: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(24)
            .map(char::from)
            .collect();

        // Port 0 lets the OS pick; Desktop clients accept any loopback port.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("could not open a local port for the OAuth redirect")?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}");

        let auth_url = format!(
            "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}\
             &code_challenge={}&code_challenge_method=S256&state={}\
             &access_type=offline&prompt=consent",
            encode(&self.creds.client_id),
            encode(&redirect_uri),
            encode(SCOPE),
            encode(&challenge),
            encode(&state),
        );

        println!(
            "  {} opening your browser to sign in with Google…",
            crate::theme::glyph::PICKAXE
        );
        println!("  if it doesn't open, paste this:\n\n  {auth_url}\n");
        let _ = open::that(&auth_url);

        let code = wait_for_code(&listener, &state)?;

        let res = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", self.creds.client_id.as_str()),
                ("client_secret", self.creds.client_secret.as_str()),
                ("code", code.as_str()),
                ("code_verifier", verifier.as_str()),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri.as_str()),
            ])
            .send()
            .await?;

        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            bail!("Google rejected the login: {body}");
        }

        let body: TokenResponse = res.json().await?;
        let refresh_token = body.refresh_token.ok_or_else(|| {
            anyhow!(
                "Google did not return a refresh token — revoke anacraft's access at \
                     https://myaccount.google.com/permissions and try again"
            )
        })?;

        Tokens {
            access_token: body.access_token,
            refresh_token,
            expires_at: Utc::now() + Duration::seconds(body.expires_in),
            account: body.id_token.as_deref().and_then(account_from_id_token),
        }
        .save()?;

        Ok(())
    }

    /// Best-effort revoke, then drop local tokens regardless.
    pub async fn logout(&self) -> Result<()> {
        if let Some(tokens) = Tokens::load()? {
            let _ = self
                .http
                .post(REVOKE_URL)
                .form(&[("token", tokens.refresh_token.as_str())])
                .send()
                .await;
        }
        Tokens::clear()
    }
}

/// Block on the single redirect hit from the browser and pull `code` out of it.
fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    for stream in listener.incoming() {
        let mut stream = stream?;
        let request_line = {
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line)?;
            line
        };

        // "GET /?code=...&state=... HTTP/1.1"
        let target = request_line.split_whitespace().nth(1).unwrap_or("/");
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);

        // Browsers often ask for /favicon.ico on the same port; ignore anything
        // that isn't the redirect we're waiting for.
        if params.is_empty() {
            respond(
                &mut stream,
                &page("Waiting", "Nothing to see here yet.", Tone::Good),
            );
            continue;
        }

        if let Some(err) = params.iter().find(|(k, _)| k == "error").map(|(_, v)| v) {
            respond(
                &mut stream,
                &page(
                    "Login cancelled",
                    "Nothing was changed. You can close this tab.",
                    Tone::Bad,
                ),
            );
            bail!("login cancelled: {err}");
        }

        let state = params
            .iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.as_str());
        if state != Some(expected_state) {
            respond(
                &mut stream,
                &page(
                    "Rejected",
                    "The redirect did not match the request that started it. \
                     Close this tab and run craft login again.",
                    Tone::Bad,
                ),
            );
            bail!("OAuth state mismatch — login aborted");
        }

        let code = params
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone())
            .context("no authorization code in redirect")?;

        respond(
            &mut stream,
            &page(
                "Logged in",
                "anacraft is connected to your Google Analytics account. \
                 You can close this tab and return to the terminal.",
                Tone::Good,
            ),
        );
        return Ok(code);
    }
    bail!("browser never completed the login")
}

fn respond(stream: &mut TcpStream, html: &str) {
    let res = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(res.as_bytes());
    let _ = stream.flush();
}

/// Whether the page is reporting success or a dead end. Only the accent
/// changes; the rest of the page is the same either way.
enum Tone {
    Good,
    Bad,
}

/// The 16x16 logo grid, the same one `scripts/gen-logo.py` draws the favicon
/// and the site mark from. Rows 0 and 15 are padding, so the SVG crops to the
/// glyph's own 14x14 extent.
const MARK: [&str; 16] = [
    "................",
    "......####......",
    ".....######.....",
    "....########....",
    "....########....",
    "...####..####...",
    "...####..####...",
    "..####....####..",
    "..####....####..",
    ".####......####.",
    ".##############.",
    ".##############.",
    ".####......####.",
    ".####......####.",
    ".####......####.",
    "................",
];

fn hex(color: ratatui::style::Color) -> String {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        // Every shipped palette is truecolor; this is only a safety net.
        _ => "#000000".to_string(),
    }
}

/// The mark as inline SVG rects, run-length encoded a row at a time so the
/// markup stays small enough to sit in a single response.
fn mark_svg(fill: &str) -> String {
    let mut out = String::from(
        "<svg viewBox=\"1 1 14 14\" width=\"52\" height=\"52\" \
         shape-rendering=\"crispEdges\" aria-hidden=\"true\">",
    );
    for (y, row) in MARK.iter().enumerate() {
        let cells: Vec<char> = row.chars().collect();
        let mut x = 0;
        while x < cells.len() {
            if cells[x] == '#' {
                let start = x;
                while x < cells.len() && cells[x] == '#' {
                    x += 1;
                }
                out.push_str(&format!(
                    "<rect x=\"{start}\" y=\"{y}\" width=\"{}\" height=\"1\" fill=\"{fill}\"/>",
                    x - start
                ));
            } else {
                x += 1;
            }
        }
    }
    out.push_str("</svg>");
    out
}

/// The browser page, drawn in whatever palette the user is running.
///
/// Deriving it from the live palette rather than hardcoding brand colours is
/// the whole point: this page cannot drift away from the dashboard the way the
/// previous hardcoded one did, and a light palette gets a readable light page
/// for free.
fn page(title: &str, body: &str, tone: Tone) -> String {
    let p = crate::theme::palette();
    let (ink, card, fg, dim, shadow) =
        (hex(p.ink), hex(p.bg), hex(p.fg), hex(p.sage), hex(p.shadow));
    let accent = match tone {
        Tone::Good => hex(p.accent),
        Tone::Bad => hex(p.coral),
    };
    let mark = mark_svg(&accent);

    // A strip of blocks under the card, in the ore vocabulary the dashboard
    // uses for a filled bar.
    let blocks: String = (0..14)
        .map(|i| {
            let c = if i < 11 { &accent } else { &shadow };
            format!("<i style=\"background:{c}\"></i>")
        })
        .collect();

    format!(
        "<!doctype html><html lang=en><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>anacraft — {title}</title>\
         <style>\
         *{{box-sizing:border-box}}\
         body{{background:{ink};color:{fg};margin:0;height:100vh;display:grid;\
         place-items:center;font-family:ui-monospace,'SF Mono',SFMono-Regular,Menlo,\
         Consolas,monospace;-webkit-font-smoothing:antialiased}}\
         .card{{background:{card};border:1px solid {shadow};border-top:3px solid {accent};\
         padding:44px 52px;text-align:center;max-width:min(92vw,460px);\
         animation:rise .28s ease-out both}}\
         svg{{display:block;margin:0 auto 22px}}\
         h1{{color:{accent};font-size:19px;font-weight:700;letter-spacing:.04em;\
         margin:0 0 10px}}\
         p{{color:{dim};font-size:13.5px;line-height:1.6;margin:0}}\
         .bar{{display:flex;gap:2px;justify-content:center;margin-top:26px}}\
         .bar i{{width:9px;height:9px;display:block}}\
         @keyframes rise{{from{{opacity:0;transform:translateY(6px)}}}}\
         @media(prefers-reduced-motion:reduce){{.card{{animation:none}}}}\
         </style>\
         <div class=card>{mark}<h1>{title}</h1><p>{body}</p>\
         <div class=bar>{blocks}</div></div>"
    )
}

/// Percent-encode everything outside the unreserved set.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((decode(k), decode(v)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::thread;

    #[test]
    fn the_page_is_drawn_from_the_active_palette() {
        // The old page hardcoded its colours, which is how it drifted away from
        // the dashboard. Pin the derivation so that cannot happen again.
        crate::theme::select("osaka-jade");
        let jade = page("Logged in", "body", Tone::Good);
        assert!(jade.contains("#2dd5b7"), "accent missing");
        assert!(jade.contains("#09100d"), "ink missing");

        crate::theme::select("catppuccin-latte");
        let latte = page("Logged in", "body", Tone::Good);
        assert!(
            !latte.contains("#09100d"),
            "a light palette must not paint the dark ground"
        );

        // The failure states differ only in the accent.
        crate::theme::select("osaka-jade");
        assert!(page("Rejected", "body", Tone::Bad).contains("#ff5345"));

        crate::theme::select("osaka-jade");
    }

    #[test]
    fn the_mark_is_cropped_to_its_glyph() {
        // Rows 0 and 15 are padding; emitting them would offset the logo inside
        // its own box.
        let svg = mark_svg("#000000");
        assert!(svg.contains(r#"viewBox="1 1 14 14""#));
        assert!(!svg.contains(r#"y="0""#), "padding row 0 was drawn");
        assert!(!svg.contains(r#"y="15""#), "padding row 15 was drawn");
        // Run-length encoding: the two solid crossbar rows are one rect each.
        assert_eq!(svg.matches(r#"width="14""#).count(), 2);
    }

    #[test]
    fn we_ask_for_one_read_only_analytics_scope_and_nothing_else_sensitive() {
        // A Google OAuth review once stalled because the consent screen listed
        // `analytics` (read+write) and `analytics.manage.users.readonly`, which
        // this app has never requested. The identity scopes added for
        // subscriptions are the non-sensitive pair and need no review; a second
        // Analytics scope still would, so pin the whole set.
        assert_eq!(
            SCOPE,
            "openid email https://www.googleapis.com/auth/analytics.readonly"
        );
        let analytics: Vec<&str> = SCOPE
            .split(' ')
            .filter(|s| s.contains("googleapis.com/auth/analytics"))
            .collect();
        assert_eq!(analytics.len(), 1, "a second Analytics scope was added");
        assert!(
            analytics[0].ends_with(".readonly"),
            "anacraft has no write path; a write scope cannot be justified"
        );
        for scope in SCOPE.split(' ') {
            assert!(
                matches!(scope, "openid" | "email") || scope.contains("/auth/analytics"),
                "unreviewed scope {scope} crept in"
            );
        }
    }

    #[test]
    fn the_account_comes_out_of_the_id_token() {
        // A real id token's middle segment: base64url, no padding.
        let payload =
            URL_SAFE_NO_PAD.encode(br#"{"sub":"110147","email":"me@example.com","aud":"x"}"#);
        let account = account_from_id_token(&format!("header.{payload}.signature")).unwrap();
        assert_eq!(account.sub, "110147");
        assert_eq!(account.email.as_deref(), Some("me@example.com"));
    }

    #[test]
    fn a_token_without_identity_is_no_identity_rather_than_a_failure() {
        // Google omits the id token when `openid` was never granted, and a
        // login from before the identity scopes has none stored. Neither is an
        // error: reports work regardless, only the cross-machine lookup needs
        // it.
        assert!(account_from_id_token("not-a-jwt").is_none());
        assert!(account_from_id_token("a.!!!!.c").is_none());
        let empty = URL_SAFE_NO_PAD.encode(br#"{"sub":""}"#);
        assert!(account_from_id_token(&format!("a.{empty}.c")).is_none());
        let no_sub = URL_SAFE_NO_PAD.encode(br#"{"email":"me@example.com"}"#);
        assert!(account_from_id_token(&format!("a.{no_sub}.c")).is_none());
    }

    #[test]
    fn stored_credentials_from_before_identity_still_load() {
        // token.json written by 0.7.x has no `account` key at all.
        let old = r#"{"access_token":"a","refresh_token":"r","expires_at":"2030-01-01T00:00:00Z"}"#;
        let tokens: Tokens = serde_json::from_str(old).unwrap();
        assert!(tokens.account.is_none());
    }

    #[test]
    fn encode_leaves_unreserved_characters_alone() {
        assert_eq!(encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
    }

    #[test]
    fn encode_escapes_url_syntax() {
        assert_eq!(
            encode("https://www.googleapis.com/auth/analytics.readonly"),
            "https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fanalytics.readonly"
        );
    }

    #[test]
    fn decode_reverses_encode() {
        for original in [
            "http://127.0.0.1:8080",
            "4/0Ab_5qL-xyz+abc",
            "a b&c=d",
            "plain",
        ] {
            assert_eq!(decode(&encode(original)), original, "round trip failed");
        }
    }

    #[test]
    fn decode_survives_malformed_escapes() {
        // A trailing or invalid escape must not panic or truncate.
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("a%2"), "a%2");
    }

    #[test]
    fn parse_query_splits_pairs() {
        let params = parse_query("code=abc123&state=xyz&scope=a%2Fb");
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], ("code".into(), "abc123".into()));
        assert_eq!(params[2], ("scope".into(), "a/b".into()));
        assert!(parse_query("").is_empty());
    }

    /// Fire a single bare HTTP request at the loopback listener.
    fn hit(port: u16, target: &str) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _ = stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes());
        let _ = stream.flush();
    }

    #[test]
    fn captures_the_authorization_code() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || hit(port, "/?code=4%2FabcXYZ&state=secret"));

        let code = wait_for_code(&listener, "secret").unwrap();
        assert_eq!(code, "4/abcXYZ");
    }

    #[test]
    fn ignores_favicon_before_the_real_redirect() {
        // Browsers routinely request /favicon.ico on the same port; that must
        // not be mistaken for the redirect and abort the login.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            hit(port, "/favicon.ico");
            thread::sleep(std::time::Duration::from_millis(50));
            hit(port, "/?code=realcode&state=secret");
        });

        assert_eq!(wait_for_code(&listener, "secret").unwrap(), "realcode");
    }

    #[test]
    fn rejects_a_mismatched_state() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || hit(port, "/?code=abc&state=attacker"));

        let err = wait_for_code(&listener, "secret").unwrap_err().to_string();
        assert!(err.contains("state mismatch"), "got: {err}");
    }

    #[test]
    fn surfaces_a_denied_consent() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || hit(port, "/?error=access_denied&state=secret"));

        let err = wait_for_code(&listener, "secret").unwrap_err().to_string();
        assert!(err.contains("access_denied"), "got: {err}");
    }
}
