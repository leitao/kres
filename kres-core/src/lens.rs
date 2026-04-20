//! Analytic lens — one slow-agent call's angle on a task.
//!
//! installs session-wide lenses from the user's prompt file; each
//! task fans out N slow-agent calls over the same gathered
//! symbols/context, and a consolidator dedupes. The `LensSpec` here
//! carries just enough for the slow-agent prompt's
//! `parallel_lenses.your_lens` routing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LensSpec {
    /// Stable slug used to reference the lens by id.
    pub id: String,
    /// Lens type — e.g. "investigate", "question", "search".
    #[serde(rename = "type", default = "default_investigate")]
    pub kind: String,
    /// Human title for this angle.
    pub name: String,
    /// Free-form guidance appended to the slow-agent prompt.
    #[serde(default)]
    pub reason: String,
}

fn default_investigate() -> String {
    "investigate".into()
}

impl LensSpec {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: default_investigate(),
            name: name.into(),
            reason: String::new(),
        }
    }

    /// A canonical slug for logging.
    pub fn brief(&self) -> String {
        format!("[{}] {}", self.kind, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_kind_is_investigate() {
        let raw = r#"{"id":"mem","name":"memory allocations"}"#;
        let l: LensSpec = serde_json::from_str(raw).unwrap();
        assert_eq!(l.kind, "investigate");
        assert_eq!(l.id, "mem");
    }

    #[test]
    fn kind_round_trip() {
        let l = LensSpec {
            id: "r".into(),
            kind: "race".into(),
            name: "race conditions".into(),
            reason: "check locks".into(),
        };
        let s = serde_json::to_string(&l).unwrap();
        assert!(s.contains("\"type\":\"race\""));
        let back: LensSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn brief_formats() {
        let l = LensSpec::new("mem", "memory");
        assert_eq!(l.brief(), "[investigate] memory");
    }
}
