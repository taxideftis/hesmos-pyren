//! The single exit-code map (code-structure §3: mapping lives in exactly one place).
//!
//! Values come from exceptions.md §8; the reason→exit pairing is documented in
//! hesmos-core's ReasonCode docs. Nothing else in the workspace may hard-code an exit
//! number — commands import these constants.

/// Success.
pub const EXIT_OK: i32 = 0;
/// Bad invocation (unknown flag, missing argument, not-implemented skeleton command).
pub const EXIT_USAGE: i32 = 2;
/// Pre-execution plan/config rejection — CE-01..CE-09, no session created.
pub const EXIT_COMPILE: i32 = 3;
/// BUDGET_EXCEEDED → session SUSPENDED with checkpoint.
pub const EXIT_BUDGET_SUSPENDED: i32 = 10;
/// MAX_HANDOFFS / REPETITIVE_HANDOFF → session HALTED.
pub const EXIT_HALTED_LOOP: i32 = 11;
/// TIMEOUT / PROVIDER_FAILURE → session HALTED (ABORTED).
pub const EXIT_HALTED_ABORTED: i32 = 12;
/// Final GATE_REJECT → session FAILED. Never reused for halts.
pub const EXIT_FAILED: i32 = 20;
/// audit_verify failed after seal → session evidence-invalid (SS-03 rule 2).
pub const EXIT_EVIDENCE_INVALID: i32 = 30;
/// SIGINT-forwarded cancellation.
pub const EXIT_SIGINT: i32 = 130;

/// The whole map as data, for tests and the `--help` epilogue. Not yet consumed by
/// commands (P0a stubs use EXIT_USAGE only) — WP-P1e wires real exits onto it.
#[allow(dead_code)]
pub const ALL: [(i32, &str); 9] = [
    (EXIT_OK, "ok"),
    (EXIT_USAGE, "usage error"),
    (EXIT_COMPILE, "compile error (CE-01..09)"),
    (EXIT_BUDGET_SUSPENDED, "budget suspended"),
    (EXIT_HALTED_LOOP, "halted (loop guard)"),
    (EXIT_HALTED_ABORTED, "halted (timeout/provider)"),
    (EXIT_FAILED, "failed (gate reject)"),
    (EXIT_EVIDENCE_INVALID, "evidence invalid (audit)"),
    (EXIT_SIGINT, "cancelled (SIGINT)"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Exit codes are a public contract (exceptions.md §8): duplicates or silent
    /// renumbering would break every script driving the CLI.
    #[test]
    fn exit_codes_are_unique_and_documented() {
        let mut codes: Vec<i32> = ALL.iter().map(|(c, _)| *c).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), ALL.len(), "exit codes must be unique");
        assert!(ALL.iter().all(|(_, label)| !label.is_empty()));
    }

    /// US-07 AC2 — the CANCELLED band and the FAILED band are different codes, as are
    /// budget-suspend and gate-reject. Scripts branch on these numbers; collapsing any
    /// pair would be an exceptions.md revision, not a refactor.
    #[test]
    fn cancelled_and_failed_bands_never_share_a_code() {
        assert_ne!(EXIT_BUDGET_SUSPENDED, EXIT_FAILED, "10 ≠ 20");
        assert_ne!(EXIT_SIGINT, EXIT_FAILED, "cancelled ≠ failed");
        assert_ne!(EXIT_HALTED_LOOP, EXIT_FAILED);
        assert_ne!(EXIT_HALTED_ABORTED, EXIT_HALTED_LOOP);
    }
}
