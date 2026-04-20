//! Model selection + thinking-budget defaults.
//!
//! Fixes bugs.md#R1: default model is `claude-opus-4-7`.
//! Fixes bugs.md#R2: thinking budget default leaves room for output
//! tokens instead of swallowing the entire max_tokens budget.

use std::path::Path;

/// A model id paired with its known output-token ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub max_output_tokens: u32,
}

impl Model {
    /// Infer the model from an API-key file name — matches the
    /// `pick_model` semantics. "opus" in the
    /// filename → Opus 4.7. Otherwise Sonnet 4.6.
    pub fn from_key_file(path: &Path) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();
        if name.contains("opus") {
            Model::opus_4_7()
        } else {
            Model::sonnet_4_6()
        }
    }

    pub fn opus_4_7() -> Self {
        Self {
            id: "claude-opus-4-7".to_string(),
            max_output_tokens: 128_000,
        }
    }

    pub fn sonnet_4_6() -> Self {
        Self {
            id: "claude-sonnet-4-6".to_string(),
            max_output_tokens: 64_000,
        }
    }

    /// Wrap an explicit model id from config; unknown ids fall back to
    /// a conservative 64k output ceiling so we don't blow up on an
    /// unexpected string.
    pub fn from_id(id: impl Into<String>) -> Self {
        let id: String = id.into();
        let max_output_tokens = match id.as_str() {
            "claude-opus-4-7" | "claude-opus-4-6" => 128_000,
            _ => 64_000,
        };
        Self {
            id,
            max_output_tokens,
        }
    }
}

/// How extended thinking is configured for a single call.
///
/// Two API shapes are in use:
/// - Legacy `{"thinking": {"type": "enabled", "budget_tokens": N}}` —
///   older models (opus-4-6, sonnet-4-6).
/// - Adaptive `{"thinking": {"type": "adaptive"},
///              "output_config": {"effort": "low|medium|high"}}` —
///   opus-4-7 and newer.
///
/// bugs.md#R2: set `thinking_budget = max_tokens - 1`
/// regardless, starving the model of output tokens. The builders below
/// always leave at least 25% of max_tokens for output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingBudget {
    /// No extended-thinking block.
    Disabled,
    /// Legacy explicit-budget thinking. Clamped to leave ≥25% of
    /// max_tokens available for output.
    LegacyBudget(u32),
    /// New adaptive thinking. The model chooses the budget; the
    /// operator picks an `effort` bias.
    Adaptive(Effort),
}

/// Effort bias passed to adaptive thinking via `output_config.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

impl ThinkingBudget {
    /// Best default for a given model id.
    ///
    /// - Models with "opus-4-7" (or later) in the id use adaptive
    ///   (medium).
    /// - Everything else uses a legacy budget sized for the output cap.
    pub fn default_for_model(model_id: &str, max_tokens: u32) -> Self {
        // Model families that require adaptive schema. Keep this list
        // conservative — when in doubt, fall back to legacy.
        let adaptive = model_id.contains("opus-4-7") || model_id.contains("opus-4-8");
        if adaptive {
            ThinkingBudget::Adaptive(Effort::Medium)
        } else {
            Self::default_legacy_for(max_tokens)
        }
    }

    /// Default sane legacy budget: `min(max_tokens / 4, 32_000)`.
    pub fn default_legacy_for(max_tokens: u32) -> Self {
        let quarter = max_tokens / 4;
        let budget = quarter.min(32_000);
        if budget == 0 {
            ThinkingBudget::Disabled
        } else {
            ThinkingBudget::LegacyBudget(budget)
        }
    }

    /// Back-compat wrapper: treat "default" as the legacy default.
    /// Used by code paths that are model-agnostic.
    pub fn default_for(max_tokens: u32) -> Self {
        Self::default_legacy_for(max_tokens)
    }

    /// Construct a legacy budget, clamping to leave at least 25% of
    /// `max_tokens` for output. Returns `Disabled` if caller passes 0.
    pub fn enabled_clamped(requested: u32, max_tokens: u32) -> Self {
        if requested == 0 {
            return ThinkingBudget::Disabled;
        }
        let reserved = max_tokens.div_ceil(4); // ceil(max/4)
        let ceiling = max_tokens.saturating_sub(reserved);
        let clamped = requested.min(ceiling);
        if clamped == 0 {
            ThinkingBudget::Disabled
        } else {
            ThinkingBudget::LegacyBudget(clamped)
        }
    }

    /// Legacy accessor — returns `Some(n)` only for LegacyBudget.
    pub fn as_budget_tokens(&self) -> Option<u32> {
        match self {
            ThinkingBudget::LegacyBudget(n) => Some(*n),
            _ => None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self, ThinkingBudget::Disabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn key_file_opus_selects_opus_4_7() {
        // bugs.md#R1
        let p = PathBuf::from("/home/user/opus.api.key");
        let m = Model::from_key_file(&p);
        assert_eq!(m.id, "claude-opus-4-7");
        assert_eq!(m.max_output_tokens, 128_000);
    }

    #[test]
    fn key_file_unknown_falls_back_to_sonnet() {
        let p = PathBuf::from("/home/user/other.key");
        let m = Model::from_key_file(&p);
        assert_eq!(m.id, "claude-sonnet-4-6");
    }

    #[test]
    fn key_file_no_name_component_falls_back_to_sonnet() {
        let p = PathBuf::from("");
        let m = Model::from_key_file(&p);
        assert_eq!(m.id, "claude-sonnet-4-6");
    }

    #[test]
    fn key_file_case_insensitive() {
        let p = PathBuf::from("/home/user/OPUS.API.KEY");
        let m = Model::from_key_file(&p);
        assert_eq!(m.id, "claude-opus-4-7");
    }

    #[test]
    fn default_thinking_budget_leaves_room_for_output() {
        // bugs.md#R2: with max_tokens=128000, the old code set
        // budget=127999, leaving 1 token for the answer. The new
        // legacy default MUST leave at least 25% of max_tokens for
        // output.
        let b = ThinkingBudget::default_legacy_for(128_000);
        let tokens = b.as_budget_tokens().unwrap();
        assert!(tokens <= 32_000, "default capped at 32000, got {tokens}");
        assert!(
            128_000 - tokens >= 128_000 / 4,
            "at least a quarter of max_tokens reserved for output"
        );
    }

    #[test]
    fn default_thinking_budget_small_max() {
        let b = ThinkingBudget::default_legacy_for(4_000);
        assert_eq!(b.as_budget_tokens(), Some(1_000));
    }

    #[test]
    fn default_for_model_picks_adaptive_for_opus_47() {
        let b = ThinkingBudget::default_for_model("claude-opus-4-7", 128_000);
        assert!(matches!(b, ThinkingBudget::Adaptive(Effort::Medium)));
    }

    #[test]
    fn default_for_model_picks_legacy_for_opus_46() {
        let b = ThinkingBudget::default_for_model("claude-opus-4-6", 128_000);
        match b {
            ThinkingBudget::LegacyBudget(n) => {
                assert!(n <= 32_000);
                assert!(128_000 - n >= 128_000 / 4);
            }
            other => panic!("expected LegacyBudget, got {:?}", other),
        }
    }

    #[test]
    fn default_for_model_unknown_defaults_to_legacy() {
        let b = ThinkingBudget::default_for_model("claude-sonnet-4-6", 64_000);
        assert!(matches!(b, ThinkingBudget::LegacyBudget(_)));
    }

    #[test]
    fn effort_strings() {
        assert_eq!(Effort::Low.as_str(), "low");
        assert_eq!(Effort::Medium.as_str(), "medium");
        assert_eq!(Effort::High.as_str(), "high");
    }

    #[test]
    fn default_thinking_budget_zero_max_is_disabled() {
        let b = ThinkingBudget::default_for(0);
        assert_eq!(b, ThinkingBudget::Disabled);
    }

    #[test]
    fn clamped_requested_budget_respects_quarter_reservation() {
        // User asks for 127999 (what the old default did); we clamp
        // to leave a quarter for output.
        let b = ThinkingBudget::enabled_clamped(127_999, 128_000);
        let tokens = b.as_budget_tokens().unwrap();
        let reserved = 128_000_u32.div_ceil(4);
        assert!(tokens <= 128_000 - reserved);
        // Legacy budget form, not adaptive.
        assert!(matches!(b, ThinkingBudget::LegacyBudget(_)));
    }

    #[test]
    fn clamped_zero_is_disabled() {
        assert_eq!(
            ThinkingBudget::enabled_clamped(0, 128_000),
            ThinkingBudget::Disabled
        );
    }

    #[test]
    fn from_id_known_values() {
        assert_eq!(Model::from_id("claude-opus-4-7").max_output_tokens, 128_000);
        assert_eq!(Model::from_id("claude-opus-4-6").max_output_tokens, 128_000);
    }

    #[test]
    fn from_id_unknown_falls_back_safely() {
        // Unknown ids get a conservative default rather than panicking.
        let m = Model::from_id("claude-future-model-x");
        assert_eq!(m.max_output_tokens, 64_000);
    }
}
