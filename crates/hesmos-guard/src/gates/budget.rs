//! budget — the Pre gate that pre-blocks execution past the envelope (SS-15 rule 4).
//!
//! The decision consumes the runner-provided snapshot ([`hesmos_core::BudgetState`]);
//! this gate never touches the ledger (code-structure §2: budget must not call gates,
//! gates must not meter). Blocking on the WARN line here would be the premature-block
//! violation of SS-15 rule 3 — warns continue execution and their budget.event emission
//! is the runner's; only a suspend line stops the call.

use hesmos_core::{GateVerdict, ReasonCode};

use crate::gate::{Gate, GateCtx};

pub struct BudgetGate;

impl Gate for BudgetGate {
    fn id(&self) -> &'static str {
        "budget"
    }

    fn check(&self, ctx: &GateCtx) -> GateVerdict {
        if ctx.budget_state.suspend_reached() {
            // reason_code = BUDGET_EXCEEDED on purpose: the runner maps this code to
            // suspend+checkpoint (Suspended), NOT to the GATE_REJECT/Failed terminal —
            // the gate's fail event carries the truth of WHY it blocked.
            GateVerdict::Reject {
                reason_code: ReasonCode::BUDGET_EXCEEDED,
                score: 0.0,
            }
        } else {
            GateVerdict::Pass { score: 1.0 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GatePhase;
    use crate::test_support::Fixture;
    use hesmos_core::NodeId;

    #[test]
    fn suspend_line_blocks_warn_line_does_not() {
        let mut fx = Fixture::new();
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Pre);
        assert!(matches!(
            BudgetGate.check(&ctx),
            GateVerdict::Pass { score: 1.0 }
        ));

        // Inside the warn band (80% of a 250K envelope): still Pass — execution continues.
        fx.budget.session_warn_limit = Some(200_000);
        fx.budget.session_suspend_limit = Some(250_000);
        fx.budget.session_spent = 200_000;
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Pre);
        assert!(matches!(
            BudgetGate.check(&ctx),
            GateVerdict::Pass { score: 1.0 }
        ));

        // At the suspend line: Reject with the budget reason (pre-block).
        fx.budget.session_spent = 250_000;
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Pre);
        assert!(matches!(
            BudgetGate.check(&ctx),
            GateVerdict::Reject {
                reason_code: ReasonCode::BUDGET_EXCEEDED,
                score: 0.0
            }
        ));
    }

    /// An agent-scope cap blocks just as a session cap does (all three scopes count).
    #[test]
    fn agent_scope_suspend_blocks_too() {
        let mut fx = Fixture::new();
        fx.budget.agent_suspend_limit = Some(1_000);
        fx.budget.agent_spent = 1_000;
        let ctx = fx.ctx(NodeId::new("n"), GatePhase::Pre);
        assert!(matches!(
            BudgetGate.check(&ctx),
            GateVerdict::Reject {
                reason_code: ReasonCode::BUDGET_EXCEEDED,
                ..
            }
        ));
    }
}
