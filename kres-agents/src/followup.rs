//! Followup types the agents use to request data.
//!
//! The shape matches the existing wire format. We accept the
//! minor variations already handles (e.g.
//! `file` vs `path` aliases) so old agent prompts continue to
//! interoperate.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Followup {
    /// "source", "callers", "callees", "search", "file", "read",
    /// "git", "question".
    #[serde(rename = "type")]
    pub kind: String,
    /// What to fetch: a symbol name, a regex, a path, etc.
    pub name: String,
    #[serde(default)]
    pub reason: String,
    /// Optional scoping path for search/file types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Followup {
    /// Return a canonical cache key so the fast agent's dedup logic
    /// has something stable to compare against.
    pub fn cache_key(&self) -> String {
        if let Some(p) = &self.path {
            format!("{}::{}::{}", self.kind, self.name, p)
        } else {
            format!("{}::{}", self.kind, self.name)
        }
    }

    /// Reason tag convention ([MISSING] / [EXTEND]) used by the todo
    /// agent to determine "is this followup resolved?".
    pub fn reason_tag(&self) -> Option<&str> {
        ["[MISSING]", "[EXTEND]", "[FLAG]"]
            .into_iter()
            .find(|&tag| self.reason.contains(tag))
            .map(|v| v as _)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let f = Followup {
            kind: "search".into(),
            name: "foo.*bar".into(),
            reason: "[EXTEND] see what calls this".into(),
            path: Some("drivers/net".into()),
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: Followup = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn cache_key_includes_path_when_present() {
        let f = Followup {
            kind: "search".into(),
            name: "x".into(),
            reason: "".into(),
            path: Some("dir".into()),
        };
        assert_eq!(f.cache_key(), "search::x::dir");
        let mut f2 = f.clone();
        f2.path = None;
        assert_eq!(f2.cache_key(), "search::x");
    }

    #[test]
    fn reason_tag_detection() {
        let f = Followup {
            kind: "source".into(),
            name: "x".into(),
            reason: "[MISSING] slow agent needs the def".into(),
            path: None,
        };
        assert_eq!(f.reason_tag(), Some("[MISSING]"));
        let g = Followup {
            kind: "source".into(),
            name: "x".into(),
            reason: "just because".into(),
            path: None,
        };
        assert_eq!(g.reason_tag(), None);
    }
}
