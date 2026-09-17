//! T4 (field-absence matrix, exhaustive) + S2 (100% reject on absence — zero
//! exceptions) at the integration seam: the validator consumes core's TYPE-4 schema
//! exactly as the router's step 1 will (TRAIT-4), and nothing in this file reaches
//! into guard internals.

use hesmos_core::{
    ArtifactRef, Assumption, Confidence, DoneCriteria, FailedApproach, HandoffContract, NodeId,
    Payload, PermissionCap, SchemaId, Sha256Hex, SnapshotSlice,
};
use hesmos_guard::{ContractJudgment, SNAPSHOT_TOKEN_LIMIT, snapshot_token_count, validate};
use std::collections::BTreeSet;

const TASK: &str = "Migrate the billing service to the new ledger";

fn contract() -> HandoffContract {
    HandoffContract {
        goal_original: TASK.into(),
        goal_current: "Move invoice rendering".into(),
        invariants: vec!["amounts never rounded".into()],
        done_criteria: Some(DoneCriteria {
            items: vec!["invoice fixture renders identically".into()],
        }),
        artifacts: Some(vec![ArtifactRef {
            path: "out/invoice.png".into(),
            sha256: Sha256Hex::parse("0".repeat(64)).expect("hex"),
        }]),
        failed_approaches: Some(vec![FailedApproach {
            approach: "regex parsing".into(),
            failed_reason: "locale-dependent".into(),
        }]),
        assumptions: Some(vec![Assumption {
            text: "ledger schema v2".into(),
            confirmed: false,
        }]),
        confidence: Some(Confidence::new(0.8).expect("in range")),
        from_node: NodeId::new("migrate"),
        to_node: Some(NodeId::new("verify")),
        permission_cap: PermissionCap {
            tools: BTreeSet::new(),
            network_allow: BTreeSet::new(),
        },
        snapshot: SnapshotSlice {
            goal_original: TASK.into(),
            prev_contract_summary: "schema done".into(),
            node_input: Payload {
                schema_id: SchemaId::new("migrate.in.v1"),
                json: serde_json::json!({ "target": "invoice" }),
            },
            shared_knowledge: vec![],
        },
    }
}

/// S2 — the matrix is exhaustive: removing ANY of the 8 body fields (each in its
/// natural absence spelling) never yields Accept. Exactly zero exceptions.
#[test]
fn s2_absence_of_any_body_field_is_never_accepted() {
    let floor = 0.6;

    // Row 1 — goal_original: creation without it is CE-05 upstream (P1a-tested); at
    // this layer only drift is expressible, and drift must reject.
    let mut c = contract();
    c.goal_original = TASK.replace("billing", "Billing");
    assert!(validate(&c, TASK, floor).is_rejection(), "row 1");

    // Row 2 — goal_current.
    let mut c = contract();
    c.goal_current = String::new();
    assert!(validate(&c, TASK, floor).is_rejection(), "row 2");

    // Row 3 — invariants (empty array = absence).
    let mut c = contract();
    c.invariants = vec![];
    assert!(validate(&c, TASK, floor).is_rejection(), "row 3");

    // Row 4 — done_criteria.
    let mut c = contract();
    c.done_criteria = None;
    assert!(validate(&c, TASK, floor).is_rejection(), "row 4");

    // Row 5 — artifacts: pass-with-consequence, NOT an accept.
    let mut c = contract();
    c.artifacts = None;
    assert!(
        matches!(
            validate(&c, TASK, floor),
            ContractJudgment::PostGateFail { field: "artifacts" }
        ),
        "row 5"
    );

    // Row 6 — failed_approaches: warn_proceed, NOT an accept.
    let mut c = contract();
    c.failed_approaches = None;
    assert!(
        matches!(
            validate(&c, TASK, floor),
            ContractJudgment::WarnProceed {
                field: "failed_approaches"
            }
        ),
        "row 6"
    );

    // Row 7 — assumptions.
    let mut c = contract();
    c.assumptions = None;
    assert!(validate(&c, TASK, floor).is_rejection(), "row 7");

    // Row 8 — confidence below threshold + absence spelling.
    let mut c = contract();
    c.confidence = Some(Confidence::new(0.59).expect("in range"));
    assert!(
        matches!(validate(&c, TASK, floor), ContractJudgment::BoundedRetry),
        "row 8 below floor"
    );
    let mut c = contract();
    c.confidence = None;
    assert!(validate(&c, TASK, floor).is_rejection(), "row 8 absent");
}

/// The exact-judgment side of the matrix (T4 oracle values, not just reject-ness).
#[test]
fn t4_matrix_judgments_are_exact_per_row() {
    let mut c = contract();
    c.done_criteria = None;
    assert_eq!(
        validate(&c, TASK, 0.6),
        ContractJudgment::RejectNoRetry {
            field: "done_criteria"
        },
        "done_criteria is the ONE no-retry row"
    );
}

/// US-09 AC1 — through a 3+ handoff chain, every contract's goal_original (body and
/// snapshot copy) stays byte-identical to the original ask; the validator accepts the
/// whole chain against the SAME task text.
#[test]
fn us09_three_hop_chain_propagates_goal_original_verbatim() {
    let hops = ["migrate", "verify", "summarize", "report"];
    let mut chain = Vec::new();
    for (i, node) in hops.iter().enumerate() {
        let mut c = contract();
        c.from_node = NodeId::new(if i == 0 { "plan" } else { hops[i - 1] });
        c.to_node = Some(NodeId::new(*node));
        chain.push(c);
    }
    assert!(chain.len() >= 3);

    for c in &chain {
        assert_eq!(
            validate(c, TASK, 0.6),
            ContractJudgment::Accept,
            "hop {} carries the original verbatim",
            c.to_node.as_ref().expect("to").as_str()
        );
        // Byte-identity holds in both copies, so the hash does too.
        assert_eq!(c.goal_original, TASK);
        assert_eq!(c.snapshot.goal_original, TASK);
    }
    // Every hop hashes the same immutable goal_original; per-hop body hashes differ
    // with goal_current etc., but the first hop's hash is stable across re-computation.
    let c0 = &chain[0];
    assert_eq!(c0.contract_hash(), chain[0].contract_hash());
    let mut tampered = contract();
    tampered.goal_original = "tampered".into();
    assert_ne!(c0.contract_hash(), tampered.contract_hash());
}

/// US-10 AC1 — the receiver gets exactly the 4 snapshot fields: schema rejects an
/// injected 5th field, and the validator works only off what survived parsing.
#[test]
fn us10_snapshot_injection_rejected_before_validation() {
    let mut value = serde_json::to_value(contract()).expect("json");
    let snapshot = value
        .get_mut("snapshot")
        .and_then(|s| s.as_object_mut())
        .expect("obj");
    snapshot.insert("shared_context".into(), serde_json::json!("whole context"));
    assert!(
        serde_json::from_value::<HandoffContract>(value).is_err(),
        "a 5th snapshot field must not parse"
    );
}

/// US-10 AC3 + S2 — provisional assumptions survive the chain marked, never as facts.
#[test]
fn us10_provisional_assumptions_stay_provisional_downstream() {
    let mut c = contract();
    c.assumptions = Some(vec![
        Assumption {
            text: "ledger v2".into(),
            confirmed: false,
        },
        Assumption {
            text: "postgres 16".into(),
            confirmed: true,
        },
    ]);

    let rendered = c.render_assumptions();
    assert!(rendered.iter().all(|r| !r.is_empty()));
    assert_eq!(rendered[0], "[PROVISIONAL] ledger v2");
    assert_eq!(rendered[1], "postgres 16");
}

/// contract_hash is a pure function of the 8 body fields: re-hash matches, envelope
/// mutation does not move it.
#[test]
fn t4_contract_hash_recalculation_matches() {
    let c = contract();
    let h1 = c.contract_hash();
    let h2 = c.contract_hash();
    assert_eq!(h1, h2);

    let mut env_only = contract();
    env_only.to_node = Some(NodeId::new("somewhere-else"));
    assert_eq!(c.contract_hash(), env_only.contract_hash());

    env_only.goal_current = "different".into();
    assert_ne!(c.contract_hash(), env_only.contract_hash());
}

/// G0 judgment input (SS-07 rule 3): the measurement is deterministic and scales past
/// the 8K limit when the slice is padded — the limit itself stays fixed.
#[test]
fn t4_snapshot_budget_measurement_feeds_g0() {
    let mut small = contract();
    small.snapshot.prev_contract_summary = String::new();
    small.snapshot.node_input.json = serde_json::json!({ "k": "v" });
    let small_count = snapshot_token_count(&small);
    assert!(small_count < SNAPSHOT_TOKEN_LIMIT);

    // Pad the summary far past 8K tokens (8192 * 4 chars) — count must follow.
    let mut big = small.clone();
    big.snapshot.prev_contract_summary = "x".repeat(SNAPSHOT_TOKEN_LIMIT as usize * 4 + 16);
    let big_count = snapshot_token_count(&big);
    assert!(
        big_count > SNAPSHOT_TOKEN_LIMIT,
        "padded slice must exceed the limit"
    );
    assert!(big_count > small_count);

    // Determinism: same slice → same count.
    assert_eq!(snapshot_token_count(&big), big_count);
}
