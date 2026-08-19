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

const SCOPE: &str = "https://www.googleapis.com/auth/analytics.readonly";
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
        let mut tokens = Tokens::load()?.context("not logged in — run `anacraft login`")?;

        if tokens.is_stale() {
            tokens = self.refresh(&tokens.refresh_token).await?;
            tokens.save()?;
        }
        Ok(tokens.access_token)
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
            bail!("session expired — run `anacraft login` again\n  ({body})");
        }

        let body: TokenResponse = res.json().await?;
        Ok(Tokens {
            access_token: body.access_token,
            // Refresh responses omit refresh_token; keep the one we have.
            refresh_token: body
                .refresh_token
                .unwrap_or_else(|| refresh_token.to_string()),
            expires_at: Utc::now() + Duration::seconds(body.expires_in),
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
            respond(&mut stream, &page("Waiting…", "Nothing to see here yet."));
            continue;
        }

        if let Some(err) = params.iter().find(|(k, _)| k == "error").map(|(_, v)| v) {
            respond(
                &mut stream,
                &page("Login cancelled", "You can close this tab."),
            );
            bail!("login cancelled: {err}");
        }

        let state = params
            .iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.as_str());
        if state != Some(expected_state) {
            respond(&mut stream, &page("Rejected", "State mismatch. Try again."));
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
                "Logged in!",
                "anacraft is connected. You can close this tab.",
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

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>anacraft</title>\
         <style>body{{background:#1d1a18;color:#e8e2d8;font-family:ui-monospace,\
         'SF Mono',Menlo,monospace;display:grid;place-items:center;height:100vh;margin:0}}\
         .card{{background:#2b2724;padding:2.5rem 3rem;\
         border:4px solid #6aaa40;text-align:center}}h1{{color:#5cdbd5;margin:0 0 .5rem;\
         letter-spacing:.05em}}p{{color:#a8a29a;margin:0}}</style>\
         <div class=card><h1>{title}</h1><p>{body}</p></div>"
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
