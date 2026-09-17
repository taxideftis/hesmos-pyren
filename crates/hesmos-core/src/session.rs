//! TYPE-5 SessionHandle — the session envelope.
//!
//! `seed`, `plan_hash` and `budget` are immutable after session start: they have no
//! setters and no mutation API (SS-05 rule 3, SS-15 rule 1). State transitions are
//! restricted to the ST-1 table ([`SessionState::can_transition_to`]); anything else is
//! an invariant violation and must crash the runner, not "sort of work".

use serde::{Deserialize, Serialize};

use crate::ids::{CommitSeq, RunId, SessionId, Sha256Hex, TeamId};
use crate::plan::BudgetEnvelope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionState {
    Init,
    Running,
    Suspended,
    Halted,
    Completed,
    /// Reserved variant with NO entry transition (W3-Part4): ST-1 defines no way in, and
    /// inventing one is forbidden. It exists so the enum mirrors the contract; a runner
    /// that produces it is a bug.
    Cancelled,
    Failed,
}

impl SessionState {
    /// ST-1 (service-sequences §1), machine-encoded. Resume-from-terminal states is
    /// intentionally absent: resume is `trace replay --at`, which mints a NEW session
    /// (fork_of lineage) whose handle starts at Init — the old handle never un-terminates.
    pub fn can_transition_to(self, to: SessionState) -> bool {
        use SessionState::*;
        matches!(
            (self, to),
            (Init, Running) | (Running, Completed | Suspended | Halted | Failed)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHandle {
    pub session_id: SessionId,
    /// Fresh on every replay fork (TYPE-5).
    pub run_id: RunId,
    /// Immutable after start (SS-05 rule 3); recorded on session.open.
    pub seed: u64,
    /// Immutable; produced by plan.compiled (SS-04 rule 6).
    pub plan_hash: Sha256Hex,
    /// Immutable envelope (SS-15 rule 1).
    pub budget: BudgetEnvelope,
    pub state: SessionState,
    /// Organization marker only — data, no RBAC (charter §5).
    pub team_id: Option<TeamId>,
    /// Replay lineage: (original session, commit point it forked at).
    pub fork_of: Option<(SessionId, CommitSeq)>,
    /// Set by trace.seal; the payload submitted to bathos audit append (SS-03).
    pub chain_head: Option<Sha256Hex>,
}

impl SessionHandle {
    /// Applies an ST-1 transition or reports it as an invariant violation. The runner
    /// turns `Err` into a crash — silent illegal transitions are the failure mode this
    /// design forbids (TYPE-5 invariant 2).
    pub fn transition(&mut self, to: SessionState) -> Result<(), crate::error::SchemaError> {
        if self.state.can_transition_to(to) {
            self.state = to;
            Ok(())
        } else {
            Err(crate::error::SchemaError::IllegalTransition(format!(
                "{:?} -> {:?}",
                self.state, to
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Sha256Hex;
    use crate::plan::BudgetEnvelope;
    use std::collections::BTreeMap;

    fn handle() -> SessionHandle {
        SessionHandle {
            session_id: SessionId::from_u128(1),
            run_id: RunId::from_u128(2),
            seed: 42,
            plan_hash: Sha256Hex::parse("b".repeat(64)).expect("hex"),
            budget: BudgetEnvelope {
                session_max_tokens: Some(250_000),
                team_max_tokens: None,
                agent_max_tokens: BTreeMap::new(),
                warn_pct: 80,
                suspend_pct: 100,
            },
            state: SessionState::Init,
            team_id: None,
            fork_of: None,
            chain_head: None,
        }
    }

    /// The legal paths of ST-1 and nothing else — notably Cancelled is unreachable and
    /// terminal states do not resurrect (resume is a new session via replay --at).
    #[test]
    fn st1_transition_table_is_exactly_legal() {
        assert!(SessionState::Init.can_transition_to(SessionState::Running));
        for terminal in [
            SessionState::Completed,
            SessionState::Suspended,
            SessionState::Halted,
            SessionState::Failed,
        ] {
            assert!(SessionState::Running.can_transition_to(terminal));
        }
        // Forbidden shapes:
        assert!(!SessionState::Init.can_transition_to(SessionState::Completed));
        assert!(!SessionState::Suspended.can_transition_to(SessionState::Running));
        assert!(!SessionState::Completed.can_transition_to(SessionState::Running));
        assert!(!SessionState::Cancelled.can_transition_to(SessionState::Running));
        assert!(!SessionState::Running.can_transition_to(SessionState::Cancelled));

        let mut h = handle();
        assert!(h.transition(SessionState::Running).is_ok());
        assert!(
            h.transition(SessionState::Running).is_err(),
            "Running->Running is not in ST-1"
        );
    }

    /// The reproduction 4-elements are all reachable from the handle (US-03 AC3).
    #[test]
    fn reproduction_elements_visible() {
        let h = handle();
        assert_eq!(h.seed, 42);
        assert_eq!(h.plan_hash.as_str(), "b".repeat(64));
        assert_eq!(h.budget.session_max_tokens, Some(250_000));
    }
}
