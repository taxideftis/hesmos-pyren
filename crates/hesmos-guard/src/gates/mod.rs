//! The 8 built-in gates (TRAIT-3 `built_in_gates`). Each module owns ONE gate; every
//! judgment is mechanical and deterministic (P1) — the rubric's semantic judging arrives
//! with the WP-P3b LLM judge, never before.
//!
//! Gate ids are the contract table's spellings ("G0", "permission", …) and are emitted
//! verbatim as the `gate_id` event attribute. Plans reference gates as `"<id>.<ver>"`
//! (e.g. `rubric.v1`); [`instantiate`] maps a reference to its built-in by the segment
//! before the first dot, case-insensitively, so plan spelling and event vocabulary stay
//! decoupled but total.
//!
//! Score convention: mechanical gates score 1.0 on pass and 0.0 on fail; only the rubric
//! produces intermediate scores (0.0..=1.0), because a threshold judgment with an
//! invented partial number is noise, not information.

pub mod budget;
pub mod contract;
pub mod done_criteria;
pub mod g0;
pub mod over_delegation;
pub mod permission;
pub mod rubric;
pub mod schema;

pub use budget::BudgetGate;
pub use contract::ContractGate;
pub use done_criteria::DoneCriteriaGate;
pub use g0::G0Gate;
pub use over_delegation::OverDelegationGate;
pub use permission::PermissionGate;
pub use rubric::RubricGate;
pub use schema::SchemaGate;

use crate::gate::Gate;

/// Builds a built-in gate from a plan gate reference (`"rubric.v1"` → rubric).
///
/// Two gates need runner-owned context: the contract gate judges against the session's
/// original task text, and the over-delegation gate against the node's `spawn_token_floor`
/// (both live on the plan/runner side — `GateCtx` deliberately carries neither, its field
/// set is fixed by TRAIT-3). `session_task` and `spawn_token_floor` are ignored by the
/// other six.
pub fn instantiate(
    plan_ref: &str,
    session_task: &str,
    spawn_token_floor: u64,
) -> Option<Box<dyn Gate>> {
    let builtin = plan_ref
        .split('.')
        .next()
        .unwrap_or(plan_ref)
        .to_ascii_lowercase();
    match builtin.as_str() {
        "g0" => Some(Box::new(G0Gate)),
        "permission" => Some(Box::new(PermissionGate)),
        "budget" => Some(Box::new(BudgetGate)),
        "schema" => Some(Box::new(SchemaGate)),
        "contract" => Some(Box::new(ContractGate::new(session_task))),
        "done_criteria" => Some(Box::new(DoneCriteriaGate)),
        "rubric" => Some(Box::new(RubricGate)),
        "over_delegation" => Some(Box::new(OverDelegationGate::new(spawn_token_floor))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full built-in table resolves; an unknown reference does not (a plan naming a
    /// nonexistent gate is a composition error, not a silent pass-through).
    #[test]
    fn all_eight_builtins_instantiate_and_unknown_ref_is_none() {
        for r in [
            "G0",
            "g0.v1",
            "permission.v1",
            "budget.v1",
            "schema.v1",
            "contract.v1",
            "done_criteria.v1",
            "rubric.v1",
            "over_delegation.v1",
        ] {
            assert!(instantiate(r, "task", 1000).is_some(), "ref {r}");
        }
        assert!(instantiate("nonexistent.v1", "task", 0).is_none());
    }
}
