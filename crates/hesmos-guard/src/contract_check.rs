//! TYPE-4 HandoffContract validator — the field-absence matrix as executable law.
//!
//! The matrix (api-contracts.md#TYPE-4) is the machine oracle for T4/S2; each row below
//! cites its row. Design points:
//!
//! - **Fixed row order** (goal_original → … → confidence): a contract with several
//!   defects is judged by the first row in matrix order, so the same defective contract
//!   always yields the same judgment (P1 — no "which check fired" nondeterminism).
//! - **Soft consequences defer, hard ones short-circuit.** `post_gate_fail` and
//!   `warn_proceed` are pass-with-consequence rows: they are remembered while scanning
//!   continues, and a later hard row (e.g. `assumptions` reject) overrides them. The
//!   first soft row seen wins if the contract is otherwise valid (artifacts beats
//!   failed_approaches, matching row order).
//! - **goal_original absence is unrepresentable** in the schema (`String`, required) —
//!   creation without it is CE-05 at plan compile (US-02/AC4). The validator therefore
//!   checks the *other* half of that invariant: byte-identity with the session task,
//!   in both the body and the snapshot slice (TYPE-4 invariant 1, US-09 AC1).
//! - This crate judges; it never executes retries or gates (WP-P1c) and never writes
//!   events (P6). [`ContractJudgment`] is the data a router / runner acts on.

use serde::{Deserialize, Serialize};

use hesmos_core::HandoffContract;

/// Matrix judgments as a closed enum — variant names mirror the matrix spellings
/// (`snake_case` serde renames keep event/audit strings identical to the contract doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractJudgment {
    /// All 8 body fields present and consistent — the next node may start (the router
    /// still applies its own loop guards afterwards, TRAIT-4 step 2).
    Accept,
    /// Matrix `reject` — contract returns to its sender; the next node never starts
    /// (SS-06 rule 6). `field` names the offending matrix row.
    Reject { field: &'static str, detail: String },
    /// Matrix `reject_no_retry` — done_criteria absent/empty. There is nothing to
    /// re-check against, so the retry path cannot exist (ADR-0002 강제조항).
    RejectNoRetry { field: &'static str },
    /// Matrix `post_gate_fail` — artifacts absent. The stage may run, but its output is
    /// handled as a post-gate failure instead of a normal completion.
    PostGateFail { field: &'static str },
    /// Matrix `warn_proceed` — failed_approaches absent (recommended field; forcing it
    /// would make first-ever tasks unrunnable, §5.3).
    WarnProceed { field: &'static str },
    /// Matrix `bounded_retry` — confidence below the floor; reuses the shared
    /// bounded-retry path, no separate loop (SS-10 rule 3). Executed by WP-P1c.
    BoundedRetry,
}

impl ContractJudgment {
    /// `true` when the contract did NOT pass validation (no next-node start).
    pub fn is_rejection(&self) -> bool {
        matches!(
            self,
            ContractJudgment::Reject { .. }
                | ContractJudgment::RejectNoRetry { .. }
                | ContractJudgment::BoundedRetry
        )
    }
}

/// Validates one contract against the session's original task text and the
/// confidence floor (the bounded-retry threshold — planner-supplied, P1c executes).
pub fn validate(
    contract: &HandoffContract,
    session_task: &str,
    confidence_floor: f32,
) -> ContractJudgment {
    // Row 1 — goal_original: byte-identity with the session task, body AND slice copy.
    // (Schema-level absence is impossible here; drift/tamper is the remaining threat.)
    if contract.goal_original != session_task {
        return ContractJudgment::Reject {
            field: "goal_original",
            detail: "body copy is not byte-identical to the session task".into(),
        };
    }
    if contract.snapshot.goal_original != session_task {
        return ContractJudgment::Reject {
            field: "goal_original",
            detail: "snapshot copy is not byte-identical to the session task".into(),
        };
    }

    // Row 2 — goal_current: absence for a String means empty/blank.
    if contract.goal_current.trim().is_empty() {
        return ContractJudgment::Reject {
            field: "goal_current",
            detail: "missing (empty)".into(),
        };
    }

    // Row 3 — invariants: empty array is judged the same as absent.
    if contract.invariants.is_empty() {
        return ContractJudgment::Reject {
            field: "invariants",
            detail: "empty array".into(),
        };
    }

    // Row 4 — done_criteria: absent OR zero items → reject, no retry path.
    match &contract.done_criteria {
        None => {
            return ContractJudgment::RejectNoRetry {
                field: "done_criteria",
            };
        }
        Some(dc) if dc.items.is_empty() => {
            return ContractJudgment::RejectNoRetry {
                field: "done_criteria",
            };
        }
        Some(_) => {}
    }

    // Rows 5–6 — soft consequences: remember the first, keep scanning.
    let soft = if contract.artifacts.is_none() {
        Some(ContractJudgment::PostGateFail { field: "artifacts" })
    } else if contract.failed_approaches.is_none() {
        Some(ContractJudgment::WarnProceed {
            field: "failed_approaches",
        })
    } else {
        None
    };

    // Row 7 — assumptions: absence is a hard reject (overrides any soft consequence).
    if contract.assumptions.is_none() {
        return ContractJudgment::Reject {
            field: "assumptions",
            detail: "missing".into(),
        };
    }

    // Row 8 — confidence: absence is a required-field reject; below the floor is the
    // matrix's bounded_retry branch (strictly below — the floor itself passes).
    match &contract.confidence {
        None => {
            return ContractJudgment::Reject {
                field: "confidence",
                detail: "missing required field".into(),
            };
        }
        Some(c) if c.get() < confidence_floor => return ContractJudgment::BoundedRetry,
        Some(_) => {}
    }

    soft.unwrap_or(ContractJudgment::Accept)
}

/// Snapshot token ceiling (SS-07 rule 3). G0 — the TRAIT-3 built-in, WP-P1c — rejects
/// handoffs above it; this measurement is its judgment input, kept here so the limit
/// and its estimator travel together.
pub const SNAPSHOT_TOKEN_LIMIT: u64 = 8 * 1024;

/// Deterministic token estimate: ~4 chars per token (story §6 sanctions an
/// approximation but requires same-input-same-judgment — a pure arithmetic function
/// satisfies that without a tokenizer dependency).
fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// Total estimated token weight of the 4-field snapshot slice — exactly what a
/// receiver may see, so exactly what G0 budgets (SS-07 rule 2 + 3).
pub fn snapshot_token_count(contract: &HandoffContract) -> u64 {
    let s = &contract.snapshot;
    estimate_tokens(&s.goal_original)
        + estimate_tokens(&s.prev_contract_summary)
        + estimate_tokens(&s.node_input.json.to_string())
        + s.shared_knowledge
            .iter()
            .map(|k| estimate_tokens(&k.key) + estimate_tokens(&k.content))
            .sum::<u64>()
}

/// Assumptions that are provisional (`confirmed == false`). The sanctioned renderer is
/// [`HandoffContract::render_assumptions`] (core) — it prefixes `[PROVISIONAL]`. This
/// probe exists so tests (and any future prompt builder) can verify nothing provisional
/// slips through unmarked: every text returned here MUST appear marked downstream
/// (US-10 AC3).
pub fn provisional_assumptions(contract: &HandoffContract) -> Vec<String> {
    contract
        .assumptions
        .iter()
        .flatten()
        .filter(|a| !a.confirmed)
        .map(|a| a.text.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::{
        ArtifactRef, Assumption, Confidence, DoneCriteria, FailedApproach, NodeId, Payload,
        PermissionCap, SchemaId, Sha256Hex, SnapshotSlice,
    };
    use std::collections::BTreeSet;

    const TASK: &str = "Write the weekly report";

    fn contract() -> HandoffContract {
        HandoffContract {
            goal_original: TASK.into(),
            goal_current: "Draft section 2".into(),
            invariants: vec!["cite sources".into()],
            done_criteria: Some(DoneCriteria {
                items: vec!["section 2 exists".into()],
            }),
            artifacts: Some(vec![ArtifactRef {
                path: "out/section2.md".into(),
                sha256: Sha256Hex::parse("a".repeat(64)).expect("hex"),
            }]),
            failed_approaches: Some(vec![FailedApproach {
                approach: "bullet points".into(),
                failed_reason: "too terse".into(),
            }]),
            assumptions: Some(vec![Assumption {
                text: "format is markdown".into(),
                confirmed: true,
            }]),
            confidence: Some(Confidence::new(0.9).expect("in range")),
            from_node: NodeId::new("draft"),
            to_node: Some(NodeId::new("verify")),
            permission_cap: PermissionCap {
                tools: BTreeSet::new(),
                network_allow: BTreeSet::new(),
            },
            snapshot: SnapshotSlice {
                goal_original: TASK.into(),
                prev_contract_summary: "n/a".into(),
                node_input: Payload {
                    schema_id: SchemaId::new("node_input.v1"),
                    json: serde_json::json!({ "q": "draft" }),
                },
                shared_knowledge: vec![],
            },
        }
    }

    #[test]
    fn complete_contract_accepts() {
        assert_eq!(validate(&contract(), TASK, 0.6), ContractJudgment::Accept);
    }

    /// Row 1 — body copy drift is rejected even when the snapshot copy is intact.
    #[test]
    fn goal_original_body_drift_rejected() {
        let mut c = contract();
        c.goal_original.push('!'); // one byte off — byte-identity, not similarity
        match validate(&c, TASK, 0.6) {
            ContractJudgment::Reject { field, .. } => assert_eq!(field, "goal_original"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    /// Row 1 — snapshot copy drift is caught separately from body drift.
    #[test]
    fn goal_original_snapshot_drift_rejected() {
        let mut c = contract();
        c.snapshot.goal_original = "Write the WEEKLY report".into();
        match validate(&c, TASK, 0.6) {
            ContractJudgment::Reject { field, .. } => assert_eq!(field, "goal_original"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    /// Rows 2–3 — String/Vec absence spellings: blank goal_current, empty invariants.
    #[test]
    fn blank_goal_current_and_empty_invariants_rejected() {
        let mut c = contract();
        c.goal_current = "   ".into();
        match validate(&c, TASK, 0.6) {
            ContractJudgment::Reject { field, .. } => assert_eq!(field, "goal_current"),
            other => panic!("expected reject, got {other:?}"),
        }

        let mut c = contract();
        c.invariants.clear();
        match validate(&c, TASK, 0.6) {
            ContractJudgment::Reject { field, detail } => {
                assert_eq!(field, "invariants");
                assert_eq!(detail, "empty array");
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    /// Row 4 — done_criteria absent OR zero items → reject_no_retry (both spellings).
    #[test]
    fn done_criteria_absent_or_empty_is_reject_no_retry() {
        let mut c = contract();
        c.done_criteria = None;
        assert_eq!(
            validate(&c, TASK, 0.6),
            ContractJudgment::RejectNoRetry {
                field: "done_criteria"
            }
        );

        let mut c = contract();
        c.done_criteria = Some(DoneCriteria { items: vec![] });
        assert_eq!(
            validate(&c, TASK, 0.6),
            ContractJudgment::RejectNoRetry {
                field: "done_criteria"
            }
        );
    }

    /// Rows 5–6 — soft consequences surface when the rest is valid.
    #[test]
    fn absent_artifacts_or_failed_approaches_pass_with_consequence() {
        let mut c = contract();
        c.artifacts = None;
        assert_eq!(
            validate(&c, TASK, 0.6),
            ContractJudgment::PostGateFail { field: "artifacts" }
        );

        let mut c = contract();
        c.failed_approaches = None;
        assert_eq!(
            validate(&c, TASK, 0.6),
            ContractJudgment::WarnProceed {
                field: "failed_approaches"
            }
        );
    }

    /// Row order — a hard reject overrides an earlier soft consequence: artifacts
    /// missing (post_gate_fail) + assumptions missing (reject) → reject.
    #[test]
    fn hard_reject_overrides_earlier_soft_consequence() {
        let mut c = contract();
        c.artifacts = None;
        c.assumptions = None;
        match validate(&c, TASK, 0.6) {
            ContractJudgment::Reject { field, .. } => assert_eq!(field, "assumptions"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    /// Row 7 — assumptions absent is a hard reject on its own.
    #[test]
    fn assumptions_absent_rejected() {
        let mut c = contract();
        c.assumptions = None;
        assert!(validate(&c, TASK, 0.6).is_rejection());
    }

    /// Row 8 — confidence boundary: strictly below the floor retries; the floor itself
    /// and 1.0 pass; 0.0 (with floor 0.0) passes; absence rejects.
    #[test]
    fn confidence_boundaries() {
        let mut c = contract();

        c.confidence = Some(Confidence::new(0.0).expect("in range"));
        assert_eq!(validate(&c, TASK, 0.0), ContractJudgment::Accept);
        assert_eq!(validate(&c, TASK, 0.01), ContractJudgment::BoundedRetry);

        c.confidence = Some(Confidence::new(0.6).expect("in range"));
        assert_eq!(
            validate(&c, TASK, 0.6),
            ContractJudgment::Accept,
            "floor passes"
        );
        assert_eq!(validate(&c, TASK, 0.61), ContractJudgment::BoundedRetry);

        c.confidence = Some(Confidence::new(1.0).expect("in range"));
        assert_eq!(validate(&c, TASK, 1.0), ContractJudgment::Accept);

        c.confidence = None;
        match validate(&c, TASK, 0.6) {
            ContractJudgment::Reject { field, .. } => assert_eq!(field, "confidence"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    /// BoundedRetry counts as a rejection surface: no judgment-free progress (the next
    /// node does not start on it).
    #[test]
    fn bounded_retry_is_a_rejection_surface() {
        let mut c = contract();
        c.confidence = Some(Confidence::new(0.2).expect("in range"));
        assert!(validate(&c, TASK, 0.6).is_rejection());
    }

    /// US-10 AC3 — provisional assumptions are enumerable and the sanctioned renderer
    /// marks every one of them; confirmed ones render bare.
    #[test]
    fn provisional_assumptions_always_marked_by_renderer() {
        let mut c = contract();
        c.assumptions = Some(vec![
            Assumption {
                text: "postgres v16".into(),
                confirmed: true,
            },
            Assumption {
                text: "single tenant".into(),
                confirmed: false,
            },
        ]);
        let provisional = provisional_assumptions(&c);
        assert_eq!(provisional, vec!["single tenant".to_string()]);

        let rendered = c.render_assumptions();
        assert_eq!(rendered[0], "postgres v16", "confirmed renders bare");
        assert_eq!(rendered[1], "[PROVISIONAL] single tenant");
        // No provisional text leaks unmarked: its bare form never appears.
        assert!(!rendered.iter().any(|r| r == "single tenant"));
    }

    /// Snapshot measurement: deterministic, char-proportional, and the limit is the
    /// documented 8K.
    #[test]
    fn snapshot_tokens_deterministic_and_bounded_by_limit_constant() {
        let c = contract();
        let first = snapshot_token_count(&c);
        assert_eq!(snapshot_token_count(&c), first, "same input → same count");
        assert_eq!(SNAPSHOT_TOKEN_LIMIT, 8192);
        // 12-char summary ⇒ ceil(12/4) = 3 — the estimator is pure arithmetic.
        assert_eq!(estimate_tokens("abcdefghijkl"), 3);
    }
}
