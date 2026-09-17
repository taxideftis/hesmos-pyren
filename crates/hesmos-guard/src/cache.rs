//! SS-18 core half — the prompt-cache invariant verifier (T12 코어부).
//!
//! The system prompt is byte-stable for the SESSION's lifetime; the core verifies the
//! hash EVERY turn and a violation halts the session immediately (S3 — no warning
//! pass-through). This module is the judgment only: it freezes the expected hash and
//! answers per turn. The HALT mechanics (violation event, state transition, zero
//! retries) live in the runner, which consumes the judgment through the orchestrator's
//! `CacheConductor` seam (D-2 — the guard type never crosses it).
//!
//! Fail-closed by contract: a session that FROZE a hash declares "this session has a
//! system prompt", so a turn reporting NO hash is itself a violation — the invariant
//! would be unverifiable for that turn, and "경고로 통과 금지" (SS-18 rule 2) forbids
//! waving it through. A session that froze nothing verifies nothing (model-neutral:
//! no prompt, no invariant — the W5 echo world).
//!
//! Special no-retry rule (exceptions.md §4): a breach of a deterministic invariant
//! cannot be retried away. The runner must NOT enter the bounded-retry loop on this
//! judgment — retrying `GATE_REJECT` on a cache violation is a contract violation.
//!
//! Not placed in W5: prompt building, cache breakpoints, deferred `--now`,
//! compression (Python side, WP-P2d / PY-7). The deferred/compression recording path
//! is decided when that feature starts (TYPE-3 revision may follow).

use hesmos_core::Sha256Hex;

/// The gate_id the runner stamps on the violation's gate.fail event. The 6-kind
/// reason-code registry gains NO entry: `CACHE_INVARIANT` is the DISPLAY phrase the
/// trace renderer derives from this gate_id (ui-spec §8.2), the event itself stays a
/// plain gate.fail with reason_code GATE_REJECT.
pub const CACHE_GATE_ID: &str = "cache";

/// The frozen per-session invariant: the system prompt's sha256, fixed at session
/// start (the prompt builder's SS-04 rule-6 hash). One per session; construction IS
/// the declaration that turns report into a per-turn obligation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptInvariant {
    expected: Sha256Hex,
}

/// Per-turn judgment. `Stable` = byte-stable prompt, execution continues; `Violated`
/// = immediate halt (never a warning, never a retry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheJudgment {
    Stable,
    Violated,
}

impl PromptInvariant {
    /// Freezes the expected hash for a session's lifetime.
    pub fn freeze(expected: Sha256Hex) -> Self {
        Self { expected }
    }

    /// Verifies ONE turn's reported prompt hash. Fail-closed: a frozen session's turn
    /// that reports no hash violates (see module doc).
    pub fn verify_turn(&self, reported: Option<&Sha256Hex>) -> CacheJudgment {
        match reported {
            Some(hash) if hash == &self.expected => CacheJudgment::Stable,
            _ => CacheJudgment::Violated,
        }
    }
}

/// Session-level entry the composition adapter calls: no frozen invariant → nothing
/// to verify (the W5 echo world reports no prompt and stays Stable forever).
pub fn verify_turn(
    frozen: Option<&PromptInvariant>,
    reported: Option<&Sha256Hex>,
) -> CacheJudgment {
    match frozen {
        None => CacheJudgment::Stable,
        Some(invariant) => invariant.verify_turn(reported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(c: char) -> Sha256Hex {
        let text: String = std::iter::repeat_n(c, 64).collect();
        Sha256Hex::parse(text).expect("hex")
    }

    /// AC5 match arm: the same hash every turn is Stable — the only continuation
    /// verdict this module can produce for a frozen session.
    #[test]
    fn matching_hash_is_stable() {
        let inv = PromptInvariant::freeze(hex('a'));
        assert_eq!(
            inv.verify_turn(Some(&hex('a'))),
            CacheJudgment::Stable,
            "byte-stable prompt keeps the session running"
        );
    }

    /// AC5 mismatch arm: any differing hash — at ANY turn — violates.
    #[test]
    fn differing_hash_violates_at_any_turn() {
        let inv = PromptInvariant::freeze(hex('a'));
        assert_eq!(inv.verify_turn(Some(&hex('b'))), CacheJudgment::Violated);
    }

    /// Fail-closed: a frozen session whose executor stops REPORTING the hash is a
    /// violation — an unverifiable turn must not pass silently (SS-18 rule 2).
    #[test]
    fn missing_report_on_frozen_session_violates() {
        let inv = PromptInvariant::freeze(hex('a'));
        assert_eq!(inv.verify_turn(None), CacheJudgment::Violated);
    }

    /// Model neutrality: a session that froze no system prompt has no invariant —
    /// nothing reported, nothing checked (the W5 echo world runs clean).
    #[test]
    fn unfrozen_session_verifies_nothing() {
        assert_eq!(verify_turn(None, None), CacheJudgment::Stable);
        assert_eq!(verify_turn(None, Some(&hex('a'))), CacheJudgment::Stable);
        // And the session-level entry delegates the frozen case to the invariant.
        let inv = PromptInvariant::freeze(hex('a'));
        assert_eq!(
            verify_turn(Some(&inv), Some(&hex('b'))),
            CacheJudgment::Violated
        );
    }

    /// The violation event's gate_id spelling — the trace renderer maps THIS value to
    /// the ui-spec §8.2 CACHE_INVARIANT phrase; a rename would silently orphan it.
    #[test]
    fn cache_gate_id_is_the_display_source() {
        assert_eq!(CACHE_GATE_ID, "cache");
    }
}
