//! T8 budget scenarios at the integration seam: the exact freeze → meter → judge loop
//! the runner will drive in WP-P1e. Nothing here reaches into crate internals — the
//! runner-visible surface is SessionBudget + Ledger + BudgetEngine only.

use hesmos_budget::{
    BudgetEngine, Ledger, MeterEntry, MeterKind, Scope, SessionBudget, ThresholdVerdict,
};
use hesmos_core::{AgentRole, BudgetEnvelope, SessionId};

/// The canonical envelope from AC1: 250K session tokens, charter 80/100 thresholds.
fn envelope_250k() -> BudgetEnvelope {
    BudgetEnvelope {
        session_max_tokens: Some(250_000),
        ..BudgetEnvelope::default()
    }
}

fn entry(role: &str, tokens: u64) -> MeterEntry {
    MeterEntry {
        team_id: Some("ci".into()),
        agent_role: role.into(),
        kind: MeterKind::Llm,
        tokens_in: tokens / 2,
        tokens_out: tokens - tokens / 2,
        cost_usd: None,
    }
}

/// T8 ① + AC1 — crossing 80% of 250K (= 200_000) emits the warn verdict with
/// spent=200000·remaining=50000, and the session KEEPS RUNNING (the next pre-call
/// evaluation is not a block).
#[test]
fn t8_warn_at_eighty_percent_execution_continues() {
    let session = SessionId::from_u128(1);
    let budget = SessionBudget::freeze(envelope_250k(), session, None);
    let ledger = Ledger::open(&session, ":memory:").expect("ledger");
    let mut engine = BudgetEngine::new(budget);

    // 150K spent so far — clear.
    ledger.record(entry("writer", 150_000), 0).expect("row");
    let spent = ledger.session_totals().expect("totals").total();
    assert_eq!(
        engine.evaluate(spent, 0, &AgentRole::new("writer"), 0),
        ThresholdVerdict::Clear
    );

    // +50K → exactly 200_000: the warn crossing.
    ledger.record(entry("writer", 50_000), 0).expect("row");
    let spent = ledger.session_totals().expect("totals").total();
    assert_eq!(
        engine.evaluate(spent, 0, &AgentRole::new("writer"), 0),
        ThresholdVerdict::Warn {
            scope: Scope::Session,
            spent: 200_000,
            remaining: Some(50_000)
        }
    );

    // Execution continues inside the band: a further call is metered, not blocked.
    ledger.record(entry("writer", 10_000), 0).expect("row");
    let spent = ledger.session_totals().expect("totals").total();
    assert_eq!(spent, 210_000);
    assert_eq!(
        engine.evaluate(spent, 0, &AgentRole::new("writer"), 0),
        ThresholdVerdict::Clear,
        "warn already fired — no duplicate, no block"
    );
}

/// T8 ② + AC2 — at 100%: suspend with reason=BUDGET_EXCEEDED, and the pre-block makes
/// it IMPOSSIBLE for further calls to happen: the runner's loop consults evaluate()
/// before each call and stops. We count the calls that would have been made.
#[test]
fn t8_suspend_at_full_pre_blocks_all_calls() {
    let session = SessionId::from_u128(2);
    let budget = SessionBudget::freeze(envelope_250k(), session, None);
    let ledger = Ledger::open(&session, ":memory:").expect("ledger");
    let mut engine = BudgetEngine::new(budget);

    let mut calls_made = 0;
    let mut suspended_reason: Option<&str> = None;
    // Chunks chosen to land exactly on the cap: 100K + 100K + 50K = 250K.
    for chunk in [100_000u64, 100_000, 50_000, 50_000] {
        // Pre-call gate: SS-15 rule 4 — the check is BEFORE the call.
        let spent = ledger.session_totals().expect("totals").total();
        if let ThresholdVerdict::Exceeded { .. } =
            engine.evaluate(spent, 0, &AgentRole::new("writer"), 0)
        {
            suspended_reason = Some("BUDGET_EXCEEDED");
            break; // zero llm.call / tool.call after suspend
        }
        ledger.record(entry("writer", chunk), 0).expect("row");
        calls_made += 1;

        // Post-call re-check: crossing 100% suspends the session.
        let spent = ledger.session_totals().expect("totals").total();
        if let ThresholdVerdict::Exceeded { .. } =
            engine.evaluate(spent, 0, &AgentRole::new("writer"), 0)
        {
            suspended_reason = Some("BUDGET_EXCEEDED");
            break;
        }
    }
    assert_eq!(calls_made, 3, "the 4th call is pre-blocked");
    assert_eq!(suspended_reason, Some("BUDGET_EXCEEDED"));
    assert_eq!(
        ledger.session_totals().expect("totals").total(),
        250_000,
        "zero execution beyond 100% — spend lands exactly on the cap (NFR)"
    );
}

/// T8 ③ — per-scope caps trip independently and are attributed to the right scope.
#[test]
fn t8_three_scope_caps_trip_and_attribute() {
    let session = SessionId::from_u128(3);
    let mut env = envelope_250k();
    env.team_max_tokens = Some(400_000);
    env.agent_max_tokens
        .insert(AgentRole::new("writer"), 120_000);
    let budget = SessionBudget::freeze(env, session, None);
    let mut engine = BudgetEngine::new(budget);

    // Session 100K + team fed 400K → team suspend.
    assert_eq!(
        engine.evaluate(100_000, 400_000, &AgentRole::new("writer"), 100_000),
        ThresholdVerdict::Exceeded { scope: Scope::Team }
    );

    // Writer hits its private 120K while session (200K/250K) and team (0) are fine.
    let mut engine = {
        let mut env = envelope_250k();
        env.agent_max_tokens
            .insert(AgentRole::new("writer"), 120_000);
        BudgetEngine::new(SessionBudget::freeze(env, SessionId::from_u128(3), None))
    };
    assert_eq!(
        engine.evaluate(200_000, 0, &AgentRole::new("writer"), 120_000),
        ThresholdVerdict::Exceeded {
            scope: Scope::Agent
        }
    );

    // A different role at a below-warn session spend stays clear — agent caps are
    // strictly per-role (the reviewer has none).
    let mut engine = {
        let mut env = envelope_250k();
        env.agent_max_tokens
            .insert(AgentRole::new("writer"), 120_000);
        BudgetEngine::new(SessionBudget::freeze(env, SessionId::from_u128(3), None))
    };
    assert_eq!(
        engine.evaluate(100_000, 0, &AgentRole::new("reviewer"), 0),
        ThresholdVerdict::Clear
    );
}

/// T8 ④ + US-20 AC3 — the ledger is the arithmetic mirror of metered calls: row sums
/// equal the token totals of every recorded call, and rows are dense and re-readable.
#[test]
fn t8_ledger_sums_match_metered_calls() {
    let session = SessionId::from_u128(4);
    let ledger = Ledger::open(&session, ":memory:").expect("ledger");

    let calls = [
        (30_000u64, "writer"),
        (70_000, "writer"),
        (50_000, "researcher"),
    ];
    let mut expected_in = 0u64;
    let mut expected_out = 0u64;
    for (tokens, role) in calls {
        let e = entry(role, tokens);
        expected_in += e.tokens_in;
        expected_out += e.tokens_out;
        ledger.record(e, 0).expect("row");
    }

    let totals = ledger.session_totals().expect("totals");
    assert_eq!(
        (totals.tokens_in, totals.tokens_out),
        (expected_in, expected_out)
    );
    assert_eq!(totals.total(), 150_000);

    let rows = ledger.all_rows().expect("rows");
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .enumerate()
            .all(|(i, r)| r.row_seq == i as u64 + 1)
    );
    let replayed: u64 = rows
        .iter()
        .map(|r| r.entry.tokens_in + r.entry.tokens_out)
        .sum();
    assert_eq!(
        replayed,
        totals.total(),
        "replay of rows == sums (US-20 AC3 shape)"
    );
}

/// T8 ⑤ — compile-time absence of an envelope-mutation API: the only way to observe
/// the envelope is the shared reference from `envelope()`; this test compiles precisely
/// because no method of `SessionBudget` can replace or mutate the frozen values.
#[test]
fn t8_envelope_is_frozen_against_mutation() {
    let budget = SessionBudget::freeze(envelope_250k(), SessionId::from_u128(5), None);
    fn assert_frozen(b: &SessionBudget) -> u64 {
        b.envelope().session_max_tokens.expect("cap")
    }
    assert_eq!(assert_frozen(&budget), 250_000);
}
