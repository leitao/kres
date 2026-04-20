//! Per-user default settings, loaded from `~/.kres/settings.json`.
//!
//! Today the file carries only per-agent default model ids:
//!
//! ```json
//! {
//!   "models": {
//!     "fast": "claude-sonnet-4-6",
//!     "slow": "claude-opus-4-7",
//!     "main": "claude-sonnet-4-6",
//!     "todo": "claude-sonnet-4-6"
//!   }
//! }
//! ```
//!
//! Precedence when picking a model for an agent role:
//!   1. the agent config's own `"model"` field (highest — per-run
//!      override);
//!   2. the matching `models.<role>` string in settings.json;
//!   3. `Model::sonnet_4_6()` (lowest — hard-coded fallback).
//!
//! A missing or empty settings.json is not an error — every field is
//! optional and the default struct just returns None from every
//! lookup.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use kres_llm::Model;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub models: Models,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Models {
    #[serde(default)]
    pub fast: Option<String>,
    #[serde(default)]
    pub slow: Option<String>,
    #[serde(default)]
    pub main: Option<String>,
    #[serde(default)]
    pub todo: Option<String>,
}

/// Which agent we're resolving a model for. Matches the per-role
/// keys in `Models`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRole {
    Fast,
    Slow,
    Main,
    Todo,
}

impl Settings {
    /// Default on-disk path: `$HOME/.kres/settings.json`. Returns
    /// None when `$HOME` is unset.
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".kres").join("settings.json"))
    }

    /// Load from an explicit path. Missing or empty file returns
    /// `Settings::default()` so callers never have to care whether
    /// the operator has populated it yet.
    pub fn load_from(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => s,
            _ => return Settings::default(),
        };
        match serde_json::from_str::<Settings>(&raw) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("settings: parse error in {}: {e}; ignoring", path.display());
                Settings::default()
            }
        }
    }

    /// Load from `~/.kres/settings.json` when `$HOME` is set,
    /// otherwise return defaults.
    pub fn load_default() -> Self {
        match Self::default_path() {
            Some(p) => Self::load_from(&p),
            None => Settings::default(),
        }
    }

    /// Model id for a role, or `None` when settings.json did not
    /// specify one.
    pub fn model_for(&self, role: ModelRole) -> Option<&str> {
        let slot = match role {
            ModelRole::Fast => &self.models.fast,
            ModelRole::Slow => &self.models.slow,
            ModelRole::Main => &self.models.main,
            ModelRole::Todo => &self.models.todo,
        };
        slot.as_deref()
    }

    /// Override the model id for a single role. `Some(id)` replaces
    /// whatever was loaded from settings.json; `None` is a no-op so
    /// callers can pass CLI `Option<String>` through directly.
    pub fn set_model(&mut self, role: ModelRole, id: Option<String>) {
        let Some(id) = id else { return };
        let slot = match role {
            ModelRole::Fast => &mut self.models.fast,
            ModelRole::Slow => &mut self.models.slow,
            ModelRole::Main => &mut self.models.main,
            ModelRole::Todo => &mut self.models.todo,
        };
        *slot = Some(id);
    }
}

/// Resolve a model for a role using the documented precedence:
/// agent config → settings.json → Model::sonnet_4_6() fallback.
pub fn pick_model(cfg_model: Option<&str>, role: ModelRole, settings: &Settings) -> Model {
    if let Some(id) = cfg_model {
        return Model::from_id(id);
    }
    if let Some(id) = settings.model_for(role) {
        return Model::from_id(id);
    }
    Model::sonnet_4_6()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let s = Settings::load_from(Path::new("/tmp/kres-settings-does-not-exist.json"));
        assert!(s.models.fast.is_none());
        assert_eq!(
            pick_model(None, ModelRole::Fast, &s).id,
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn settings_fills_in_when_cfg_is_silent() {
        let dir = std::env::temp_dir().join(format!("kres-settings-fills-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        std::fs::write(
            &p,
            r#"{"models":{"slow":"claude-opus-4-7","main":"claude-sonnet-4-6"}}"#,
        )
        .unwrap();
        let s = Settings::load_from(&p);
        assert_eq!(pick_model(None, ModelRole::Slow, &s).id, "claude-opus-4-7");
        assert_eq!(
            pick_model(None, ModelRole::Main, &s).id,
            "claude-sonnet-4-6"
        );
        // fast role has nothing in settings → falls back to sonnet_4_6.
        assert_eq!(
            pick_model(None, ModelRole::Fast, &s).id,
            "claude-sonnet-4-6"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cfg_model_always_wins() {
        let s = Settings {
            models: Models {
                slow: Some("claude-opus-4-7".into()),
                ..Default::default()
            },
        };
        assert_eq!(
            pick_model(Some("claude-sonnet-4-6"), ModelRole::Slow, &s).id,
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn empty_file_yields_defaults() {
        let dir = std::env::temp_dir().join(format!("kres-settings-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        std::fs::write(&p, "").unwrap();
        let s = Settings::load_from(&p);
        assert!(s.models.fast.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
