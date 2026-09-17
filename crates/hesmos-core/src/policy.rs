//! PolicySet — discipline as data (PT-4).
//!
//! The numbers are charter §6 values and nothing else: 20 / 8 / 2 / 8K / 80 / 100. A new
//! number requires a charter change, not a default invented in code. `min_confidence`
//! defaults to 0.0 (no gate) because charter §6 fixes no confidence threshold — raising
//! it is a plan/policy decision, not a core default (avoids inventing a number).

use serde::{Deserialize, Serialize};

/// Runtime thresholds injected into gates and the loop guard (SS-11 rule 4: thresholds
/// come from here, never from prompts or hard-coded gate internals).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySet {
    /// Cumulative handoff cap → MAX_HANDOFFS halt.
    pub max_handoffs: u32,
    /// Ping-pong observation window (A→B→A detection) → REPETITIVE_HANDOFF halt.
    pub ping_pong_window: u32,
    /// Bounded retry budget for post-gate Retry AND LLM-call retries (same knob, fixed
    /// backoff — no jitter, exceptions.md §4).
    pub bounded_retry: u8,
    /// G0 transfer budget: snapshot total ≤ 8K tokens (SS-07 rule 3).
    pub max_transfer_tokens: u64,
    /// Budget warn level (charter §6).
    pub budget_warn_pct: u8,
    /// Budget suspend level (charter §6).
    pub budget_suspend_pct: u8,
    /// Confidence floor for handoff; 0.0 disables the branch (see module doc).
    pub min_confidence: f32,
}

impl Default for PolicySet {
    fn default() -> Self {
        Self {
            max_handoffs: 20,
            ping_pong_window: 8,
            bounded_retry: 2,
            max_transfer_tokens: 8 * 1024,
            budget_warn_pct: 80,
            budget_suspend_pct: 100,
            min_confidence: 0.0,
        }
    }
}

/// Per-plan overrides. `None` = inherit the PolicySet default; there is deliberately no
/// way to express "unset a cap" — only to tighten or raise a known knob.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyOverrides {
    pub max_handoffs: Option<u32>,
    pub ping_pong_window: Option<u32>,
    pub bounded_retry: Option<u8>,
    pub max_transfer_tokens: Option<u64>,
    pub budget_warn_pct: Option<u8>,
    pub budget_suspend_pct: Option<u8>,
    pub min_confidence: Option<f32>,
}

impl PolicySet {
    /// Applies overrides: `self` supplies every unspecified value.
    pub fn apply(&self, o: &PolicyOverrides) -> PolicySet {
        PolicySet {
            max_handoffs: o.max_handoffs.unwrap_or(self.max_handoffs),
            ping_pong_window: o.ping_pong_window.unwrap_or(self.ping_pong_window),
            bounded_retry: o.bounded_retry.unwrap_or(self.bounded_retry),
            max_transfer_tokens: o.max_transfer_tokens.unwrap_or(self.max_transfer_tokens),
            budget_warn_pct: o.budget_warn_pct.unwrap_or(self.budget_warn_pct),
            budget_suspend_pct: o.budget_suspend_pct.unwrap_or(self.budget_suspend_pct),
            min_confidence: o.min_confidence.unwrap_or(self.min_confidence),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Defaults ARE the charter §6 numbers — a test so the baseline can't drift silently.
    #[test]
    fn defaults_are_charter_values() {
        let p = PolicySet::default();
        assert_eq!(
            (p.max_handoffs, p.ping_pong_window, p.bounded_retry),
            (20, 8, 2)
        );
        assert_eq!(p.max_transfer_tokens, 8 * 1024);
        assert_eq!((p.budget_warn_pct, p.budget_suspend_pct), (80, 100));
    }

    /// Overrides change only what they name.
    #[test]
    fn override_partial_application() {
        let applied = PolicySet::default().apply(&PolicyOverrides {
            max_handoffs: Some(5),
            ..Default::default()
        });
        assert_eq!(applied.max_handoffs, 5);
        assert_eq!(applied.ping_pong_window, 8);
        assert_eq!(applied.bounded_retry, 2);
    }
}
