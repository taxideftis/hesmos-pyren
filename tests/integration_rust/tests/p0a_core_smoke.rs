//! P0a contract smoke tests — the cross-crate invariants that every downstream consumer
//! (Stephen's FFI/pydantic mirror, Andrew's HTTP shapes) depends on. These drive the
//! public `hesmos-core` API only (D-4); deeper scenarios (seed reproduction, loop
//! guards, seal) arrive with WP-P1e.

use hesmos_core::{
    BudgetEnvelope, CorrelationId, Envelope, EnvelopeId, EnvelopeKind, NodeId, Payload, PolicySet,
    SchemaId, SessionHandle, SessionState, Sha256Hex, Taint, TaintSource, canonical_bytes,
};

fn sample() -> Envelope {
    Envelope {
        id: EnvelopeId::from_u128(1),
        from: NodeId::new("a"),
        to: NodeId::new("b"),
        kind: EnvelopeKind::Task,
        payload: Payload {
            schema_id: SchemaId::new("draft.v1"),
            json: serde_json::json!({ "text": "hello" }),
        },
        correlation_id: CorrelationId::from_u128(2),
        taint: Taint::Tainted {
            source: TaintSource {
                origin: "web.search".into(),
            },
        },
    }
}

/// US-01 AC1 (Rust side): serde roundtrip reproduces the exact canonical bytes — the
/// guarantee the Python mirror and every hash input rely on.
#[test]
fn envelope_roundtrip_is_byte_identical() {
    let bytes = canonical_bytes(&sample());
    let back: Envelope = serde_json::from_slice(&bytes).expect("parse own canonical bytes");
    assert_eq!(canonical_bytes(&back), bytes);
}

/// US-01 AC2: a payload missing a mandatory field is refused with the field named, and
/// nothing is constructed — rejection happens before any core state can be touched.
#[test]
fn missing_field_is_rejected_by_name() {
    let mut value: serde_json::Value =
        serde_json::from_slice(&canonical_bytes(&sample())).expect("json");
    value.as_object_mut().expect("obj").remove("taint");
    let err = serde_json::from_value::<Envelope>(value).expect_err("must reject");
    assert!(err.to_string().contains("taint"), "violation named: {err}");
}

/// ST-1 across the handle API: legal path Init→Running→Completed, and no terminal
/// resurrection (resume is a new session via replay --at, W3-3).
#[test]
fn session_transitions_follow_st1() {
    let mut handle = SessionHandle {
        session_id: hesmos_core::SessionId::from_u128(1),
        run_id: hesmos_core::RunId::from_u128(2),
        seed: 42,
        plan_hash: Sha256Hex::parse("b".repeat(64)).expect("hex"),
        budget: BudgetEnvelope::default(),
        state: SessionState::Init,
        team_id: None,
        fork_of: None,
        chain_head: None,
    };
    handle
        .transition(SessionState::Running)
        .expect("Init->Running");
    handle
        .transition(SessionState::Completed)
        .expect("Running->Completed");
    assert!(handle.transition(SessionState::Running).is_err());
}

/// Charter §6 numbers are frozen in the default policy — the baseline downstream WPs
/// gate against.
#[test]
fn policy_defaults_are_charter_values() {
    let p = PolicySet::default();
    assert_eq!(
        (p.max_handoffs, p.ping_pong_window, p.bounded_retry),
        (20, 8, 2)
    );
    assert_eq!(p.max_transfer_tokens, 8 * 1024);
}
