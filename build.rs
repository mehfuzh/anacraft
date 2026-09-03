//! Build-time configuration.
//!
//! Cargo does not track the env vars read by `option_env!`, so a rebuild after
//! changing the baked-in OAuth client or subscription project would silently
//! reuse the previous values. Declaring them here keeps a local `cargo build`
//! honest.
//!
//! A local `.env` is read too, so working on the paid path does not mean
//! exporting keys into every shell. It is gitignored, and a real environment
//! variable always wins over it — CI sets these from repository secrets and
//! must not be second-guessed by a file that happens to be lying around.

use std::fs;

/// Everything the binary may have baked in. Anything else in `.env` is left
/// alone: this is a build script, not a general-purpose dotenv loader.
const BAKED: [&str; 4] = [
    "ANACRAFT_OAUTH_CLIENT_ID",
    "ANACRAFT_OAUTH_CLIENT_SECRET",
    "ANACRAFT_SUPABASE_URL",
    "ANACRAFT_SUPABASE_KEY",
];

fn main() {
    for var in BAKED {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!("cargo:rerun-if-changed=.env");

    let Ok(raw) = fs::read_to_string(".env") else {
        return;
    };

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `export KEY=value` is what people paste out of a shell.
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !BAKED.contains(&key) || std::env::var_os(key).is_some_and(|v| !v.is_empty()) {
            continue;
        }
        // Quotes are the shell's, not the value's.
        let value = value.trim().trim_matches('"').trim_matches('\'');
        println!("cargo:rustc-env={key}={value}");
    }
}
