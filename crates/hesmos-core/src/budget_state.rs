//! Runtime budget snapshot carried into gate contexts (TRAIT-3 `GateCtx.budget_state`).
//!
//! A plain value, deliberately: the runner meters spends in hesmos-budget and hands this
//! immutable snapshot to each pre-gate evaluation, so no gate ever touches the ledger and
//! budget cannot call gates (code-structure §2 guard-order rule — the runner conducts).
//! Guard and orchestrator can both name this type because D-2 makes core the only shared
//! dependency.

use serde::{Deserialize, Serialize};

/// Spend vs. limit lines for the three SS-15 scopes, at the moment of the check.
/// `None` = no cap on that line (cannot fire); a `Some(0)` cap blocks from the first
/// token — the distinction is semantic and load-bearing (WP-P1d).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetState {
    pub session_spent: u64,
    pub session_warn_limit: Option<u64>,
    pub session_suspend_limit: Option<u64>,
    /// Team spend spans every session in the team; the runner sums it across ledgers.
    pub team_spent: u64,
    pub team_suspend_limit: Option<u64>,
    pub agent_spent: u64,
    pub agent_suspend_limit: Option<u64>,
}

impl BudgetState {
    /// SS-15 rule 4 (pre-block): any scope at/over its suspend line must stop execution
    /// BEFORE the next llm.call/tool.call, never after accounting.
    pub fn suspend_reached(&self) -> bool {
        Self::line_reached(self.session_suspend_limit, self.session_spent)
            || Self::line_reached(self.team_suspend_limit, self.team_spent)
            || Self::line_reached(self.agent_suspend_limit, self.agent_spent)
    }

    /// SS-15 rule 3: the session warn line is crossed (the warn EVENT is the runner's —
    /// one per scope — gates only consume the boolean).
    pub fn session_warn_reached(&self) -> bool {
        Self::line_reached(self.session_warn_limit, self.session_spent)
    }

    fn line_reached(limit: Option<u64>, spent: u64) -> bool {
        limit.is_some_and(|limit| spent >= limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(
        session_spent: u64,
        session_warn: Option<u64>,
        session_suspend: Option<u64>,
    ) -> BudgetState {
        BudgetState {
            session_spent,
            session_warn_limit: session_warn,
            session_suspend_limit: session_suspend,
            team_spent: 0,
            team_suspend_limit: None,
            agent_spent: 0,
            agent_suspend_limit: None,
        }
    }

    /// The uncapped line never fires; the zero cap blocks from the first token — the
    /// None/Some(0) distinction survives the snapshot boundary.
    #[test]
    fn none_is_uncapped_and_zero_cap_blocks_immediately() {
        assert!(!state(250_000, None, None).session_warn_reached());
        assert!(state(0, Some(0), Some(0)).suspend_reached());
    }

    #[test]
    fn suspend_lines_across_scopes_trip_independently() {
        let mut s = state(100, Some(80), Some(200));
        assert!(!s.suspend_reached());
        assert!(s.session_warn_reached());

        s.agent_suspend_limit = Some(50);
        assert!(!s.suspend_reached(), "agent at 0 is below its line");

        s.agent_spent = 50;
        assert!(s.suspend_reached(), "agent line reached — pre-block");
    }
}
