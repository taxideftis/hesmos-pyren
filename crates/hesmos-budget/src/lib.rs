//! hesmos-budget — metering only, never intervention (code-structure §1/§3).
//!
//! Owns the frozen budget envelope, the token ledger (three scopes: session/team/agent)
//! and the warn(80)/suspend(100) threshold judgment. It accounts for llm.call/token
//! events; it never causes them — the runner suspends on the budget's verdict.
//!
//! Collaboration is one-way through the runner: the runner meters a finished call into
//! the ledger, evaluates thresholds, and carries the resulting `BudgetState` snapshot
//! into pre-gate `GateCtx` (WP-P1c). This crate never calls gates and never writes
//! events — budget.event emission is the runner's, via PORT-1 (P6).

mod envelope;
mod ledger;
mod thresholds;

// D-7: the public surface is the re-export list below; the modules themselves are private.
pub use envelope::SessionBudget;
pub use ledger::{Ledger, LedgerError, MeterEntry, MeterKind, MeterRow, TokenTotals};
pub use thresholds::{BudgetEngine, BudgetLevel, Scope, ThresholdVerdict};

/// Compile-time proof of the D-2 dependency direction: the crate reaches the frozen
/// envelope whose thresholds it will enforce from WP-P1d on.
#[cfg(test)]
mod p0a_wiring {
    #[test]
    fn envelope_thresholds_are_charter_frozen() {
        let b = hesmos_core::BudgetEnvelope::default();
        assert_eq!((b.warn_pct, b.suspend_pct), (80, 100));
    }
}

#[cfg(test)]
mod p1d_wiring {
    /// The freeze → meter → judge chain assembles through the crate root — the exact
    /// seam the runner will drive in WP-P1e.
    #[test]
    fn freeze_meter_judge_chain_assembles() {
        use hesmos_core::{AgentRole, BudgetEnvelope, SessionId};

        let budget = crate::SessionBudget::freeze(
            BudgetEnvelope {
                session_max_tokens: Some(1000),
                ..BudgetEnvelope::default()
            },
            SessionId::from_u128(1),
            None,
        );
        let ledger = crate::Ledger::open(&SessionId::from_u128(1), ":memory:").expect("ledger");
        ledger
            .record(
                crate::MeterEntry {
                    team_id: None,
                    agent_role: "writer".into(),
                    kind: crate::MeterKind::Llm,
                    tokens_in: 450,
                    tokens_out: 450,
                    cost_usd: None,
                },
                0,
            )
            .expect("row");

        let mut engine = crate::BudgetEngine::new(budget);
        let spent = ledger.session_totals().expect("totals").total();
        assert_eq!(
            engine.evaluate(spent, 0, &AgentRole::new("writer"), spent),
            crate::ThresholdVerdict::Warn {
                scope: crate::Scope::Session,
                spent: 900,
                remaining: Some(100)
            }
        );
    }
}
