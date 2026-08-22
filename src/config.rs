//! On-disk state, split by sensitivity.
//!
//! * `~/.config/anacraft/config.toml` — properties and their settings. Hand
//!   editable, safe to commit to a dotfile repo.
//! * `~/.anacraft/token.json` — OAuth tokens, written 0600.
//!
//! Keeping them apart matters: people sync `~/.config` to public repos, and a
//! refresh token has no business travelling with it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Where tokens and a user-supplied `client.json` live. Created on demand.
pub fn home() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not locate a home directory")?
        .join(".anacraft");
    if !dir.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(dir)
}

/// `$XDG_CONFIG_HOME/anacraft` when set, else `~/.config/anacraft`.
pub fn config_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(raw) if !raw.is_empty() => PathBuf::from(raw),
        _ => dirs::home_dir()
            .context("could not locate a home directory")?
            .join(".config"),
    };
    let dir = base.join("anacraft");
    if !dir.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(dir)
}

/// One GA4 property and the settings that follow it around. Every setting is
/// optional: unset means "inherit the global default", which is what lets a
/// config hold a bare id and still work.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Property {
    /// Numeric GA4 property id, e.g. "397412345".
    pub id: String,
    /// Property name as GA reports it, cached to save an Admin API round trip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Short nickname shown in the TUI switcher in place of `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Palette for this property, so work and personal can look different.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Lookback window the dashboard opens with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
    /// Seconds between report refreshes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<u64>,
    /// Seconds between realtime polls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_refresh: Option<u64>,
}

impl Property {
    /// What to show in the switcher: nickname, else GA's name, else the id.
    pub fn display(&self) -> String {
        self.label
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| format!("property {}", self.id))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// Id of the property the dashboard opens on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Palette used by any property that doesn't name its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Serialises as TOML `[[property]]` blocks.
    #[serde(default, rename = "property", skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<Property>,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.toml"))
    }

    /// The pre-0.4 location, kept only so `load` can migrate off it.
    fn legacy_path() -> Result<PathBuf> {
        Ok(home()?.join("config.json"))
    }

    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        if path.exists() {
            let raw =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            // A corrupt config should not brick the CLI; fall back to empty.
            return Ok(toml::from_str(&raw).unwrap_or_default());
        }

        // First run after upgrading: fold the old single-property JSON into the
        // new shape and write it out, so this only happens once.
        let migrated = Self::from_legacy()?;
        if !migrated.properties.is_empty() {
            migrated.save()?;
        }
        Ok(migrated)
    }

    /// Read `~/.anacraft/config.json` as a one-property config.
    fn from_legacy() -> Result<Config> {
        #[derive(Deserialize)]
        struct Legacy {
            property_id: Option<String>,
            property_name: Option<String>,
            theme: Option<String>,
        }

        let path = Self::legacy_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = fs::read_to_string(&path)?;
        let Ok(old) = serde_json::from_str::<Legacy>(&raw) else {
            return Ok(Config::default());
        };

        let mut cfg = Config {
            active: old.property_id.clone(),
            theme: old.theme,
            properties: Vec::new(),
        };
        if let Some(id) = old.property_id {
            cfg.properties.push(Property {
                id,
                name: old.property_name,
                ..Property::default()
            });
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let body = toml::to_string_pretty(self)?;
        let raw = format!(
            "# anacraft configuration\n\
             # Properties are switched in the dashboard with Tab; every key under\n\
             # [[property]] is optional and falls back to the global default.\n\n{body}"
        );
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&Property> {
        let id = normalize(id);
        self.properties.iter().find(|p| p.id == id)
    }

    /// The active property's entry, falling back to the first configured one so
    /// a hand-written config without `active` still opens.
    pub fn active_property(&self) -> Option<&Property> {
        self.active
            .as_deref()
            .and_then(|id| self.find(id))
            .or_else(|| self.properties.first())
    }

    /// Add or update an entry, preserving any settings already on it, and make
    /// it the one the dashboard opens on.
    pub fn upsert(&mut self, id: &str, name: Option<String>) -> &mut Property {
        let id = normalize(id);
        let index = match self.properties.iter().position(|p| p.id == id) {
            Some(i) => i,
            None => {
                self.properties.push(Property {
                    id: id.clone(),
                    ..Property::default()
                });
                self.properties.len() - 1
            }
        };
        if name.is_some() {
            self.properties[index].name = name;
        }
        self.active = Some(id);
        &mut self.properties[index]
    }

    /// Resolve the property to query: explicit `--property` wins, then
    /// `ANACRAFT_PROPERTY_ID`, then whatever is active on disk.
    pub fn resolve_property(&self, flag: Option<&str>) -> Result<String> {
        if let Some(p) = flag {
            return Ok(normalize(p));
        }
        if let Ok(p) = std::env::var("ANACRAFT_PROPERTY_ID") {
            if !p.trim().is_empty() {
                return Ok(normalize(&p));
            }
        }
        self.active_property()
            .map(|p| p.id.clone())
            .context("no property selected — run `anacraft props` to pick one")
    }

    /// Palette for a property: its own, else the global default.
    pub fn theme_for(&self, id: &str) -> Option<&str> {
        self.find(id)
            .and_then(|p| p.theme.as_deref())
            .or(self.theme.as_deref())
    }
}

/// Accept `123456`, `properties/123456`, or a pasted GA URL fragment.
pub fn normalize(raw: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_the_properties_prefix() {
        assert_eq!(normalize(" properties/397412345 "), "397412345");
        assert_eq!(normalize("397412345"), "397412345");
    }

    #[test]
    fn round_trips_through_toml() {
        let cfg = Config {
            active: Some("111".into()),
            theme: Some("osaka-jade".into()),
            properties: vec![
                Property {
                    id: "111".into(),
                    name: Some("anacraft.dev".into()),
                    label: Some("site".into()),
                    theme: Some("catppuccin".into()),
                    days: Some(14),
                    refresh: Some(60),
                    live_refresh: Some(5),
                },
                Property {
                    id: "222".into(),
                    ..Property::default()
                },
            ],
        };
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back.active.as_deref(), Some("111"));
        assert_eq!(back.properties, cfg.properties);
    }

    #[test]
    fn a_bare_property_inherits_the_global_theme() {
        let cfg = Config {
            active: Some("222".into()),
            theme: Some("osaka-jade".into()),
            properties: vec![
                Property {
                    id: "111".into(),
                    theme: Some("catppuccin".into()),
                    ..Property::default()
                },
                Property {
                    id: "222".into(),
                    ..Property::default()
                },
            ],
        };
        assert_eq!(cfg.theme_for("111"), Some("catppuccin"));
        assert_eq!(cfg.theme_for("222"), Some("osaka-jade"));
    }

    #[test]
    fn active_falls_back_to_the_first_entry() {
        // A hand-written config need not name an active property.
        let cfg = Config {
            active: None,
            theme: None,
            properties: vec![Property {
                id: "999".into(),
                ..Property::default()
            }],
        };
        assert_eq!(cfg.resolve_property(None).unwrap(), "999");
    }

    #[test]
    fn upsert_keeps_existing_settings() {
        let mut cfg = Config::default();
        cfg.upsert("111", Some("First".into())).theme = Some("catppuccin".into());
        // Re-selecting the property must not wipe the theme set on it.
        cfg.upsert("properties/111", Some("Renamed".into()));

        assert_eq!(cfg.properties.len(), 1);
        assert_eq!(cfg.properties[0].theme.as_deref(), Some("catppuccin"));
        assert_eq!(cfg.properties[0].name.as_deref(), Some("Renamed"));
        assert_eq!(cfg.active.as_deref(), Some("111"));
    }

    #[test]
    fn display_prefers_the_label() {
        let mut p = Property {
            id: "111".into(),
            ..Property::default()
        };
        assert_eq!(p.display(), "property 111");
        p.name = Some("anacraft.dev".into());
        assert_eq!(p.display(), "anacraft.dev");
        p.label = Some("site".into());
        assert_eq!(p.display(), "site");
    }
}
