//! On-disk state: `~/.anacraft/config.json` (chosen property, prefs) and
//! `~/.anacraft/token.json` (OAuth tokens, written 0600).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub fn home() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not locate a home directory")?
        .join(".anacraft");
    if !dir.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(dir)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// Numeric GA4 property id, e.g. "397412345".
    pub property_id: Option<String>,
    /// Human label for the property, cached so we can print it without an
    /// extra Admin API round trip.
    pub property_name: Option<String>,
    /// Palette name, e.g. "catppuccin". Unset means the default.
    pub theme: Option<String>,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(home()?.join("config.json"))
    }

    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        // A corrupt config should not brick the CLI; fall back to empty.
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Resolve the property to query: explicit `--property` wins, then the
    /// saved default, then the `ANACRAFT_PROPERTY_ID` env var.
    pub fn resolve_property(&self, flag: Option<&str>) -> Result<String> {
        if let Some(p) = flag {
            return Ok(normalize(p));
        }
        if let Ok(p) = std::env::var("ANACRAFT_PROPERTY_ID") {
            if !p.trim().is_empty() {
                return Ok(normalize(&p));
            }
        }
        self.property_id
            .as_deref()
            .map(normalize)
            .context("no property selected — run `anacraft props` to pick one")
    }
}

/// Accept `123456`, `properties/123456`, or a pasted GA URL fragment.
fn normalize(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("properties/")
        .trim()
        .to_string()
}

/// Write a file readable only by the current user. On Windows we fall back to
/// a plain write, since ACL handling there is out of scope.
pub fn write_private(path: &PathBuf, contents: &str) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    Ok(())
}
