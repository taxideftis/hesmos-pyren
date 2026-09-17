//! S1 structural reproducibility (WP-P1a): same (plan, seed) + same stage outputs →
//! identical compiled structure (waves, order, plan_hash) and identical commit
//! receipts — with no LLM in the loop (provider double = pure function of node id and
//! the PRNG, itself deterministic).
//!
//! The full S1 (byte-identical events.jsonl between two runs) lands in WP-P1e, when
//! the runner funnels everything through the EventSink; here we pin the engine-level
//! invariants that make it possible.

use hesmos_core::{
    Confidence, CorrelationId, Envelope, EnvelopeId, EnvelopeKind, NodeId, Payload, SchemaId,
    Sha256Hex, Taint, canonical_bytes, canonical_sha256,
};
use hesmos_orchestrator::{DeterministicEngine, GraphEngine, SplitMix64, StageOutput, parse_plan};

const PLAN: &str = r#"
name: s1-repro
task: produce the weekly report
pattern: graph
stages:
  - id: fetch
    profile: {role: researcher, model: glm-5.3-flash, tools: [web.search]}
    input_schema: fetch.v1
    done_criteria: {items: [notes]}
  - id: summarize
    profile: {role: researcher, model: glm-5.3-flash}
    input_schema: summary.v1
    done_criteria: {items: [summary]}
  - id: verify
    profile: {role: reviewer, model: glm-5.3-flash}
    input_schema: check.v1
    done_criteria: {items: [verdict]}
  - id: draft
    profile: {role: writer, model: glm-5.3-flash}
    input_schema: draft.v1
    done_criteria: {items: [draft]}
    depends: [fetch, summarize]
flow: "fetch -> summarize, verify"
"#;

/// Provider double: output content is a pure function of (node, n) — stand-in for the
/// model call, which WP-P1e replaces behind the same StageOutput seam.
fn output(node: &NodeId, n: u128) -> StageOutput {
    StageOutput {
        node: node.clone(),
        envelope: Envelope {
            id: EnvelopeId::from_u128(n),
            from: node.clone(),
            to: NodeId::new("<sink>"),
            kind: EnvelopeKind::Result,
            payload: Payload {
                schema_id: SchemaId::new("stage.result.v1"),
                json: serde_json::json!({ "node": node.as_str(), "seq": n }),
            },
            correlation_id: CorrelationId::from_u128(n),
            taint: Taint::Clean,
        },
        confidence: Confidence::new(0.85).expect("in range"),
    }
}

/// Drives one full session deterministically, returning the structural fingerprint:
/// canonical bytes of the wave list and every receipt's (seq, node, event_hash).
fn run_once(seed: u64) -> (Vec<u8>, Vec<(u64, String, Sha256Hex)>) {
    let plan = parse_plan(PLAN).expect("plan compiles");
    let engine = DeterministicEngine;
    let mut graph = engine.compile(&plan, seed).expect("compile");

    let wave_bytes = canonical_bytes(&graph.waves().to_vec());
    let mut receipts = Vec::new();
    let mut n = 0u128;
    while let Some(wave) = engine.next_wave(&graph) {
        for node in &wave.nodes {
            n += 1;
            let r = engine.commit(&mut graph, output(node, n));
            receipts.push((r.commit_seq.0, node.as_str().to_string(), r.event_hash));
        }
    }
    (wave_bytes, receipts)
}

#[test]
fn s1_same_plan_and_seed_same_structure_and_receipts() {
    let (waves_a, receipts_a) = run_once(4242);
    let (waves_b, receipts_b) = run_once(4242);

    assert_eq!(waves_a, waves_b, "wave list + order byte-identical");
    assert_eq!(receipts_a, receipts_b, "commit receipts identical");

    // 4 stages, 2 waves: {fetch, verify} then {summarize... } — fan-out merges.
    assert_eq!(receipts_a.len(), 4);
    assert_eq!(
        receipts_a.iter().map(|(s, _, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "commit seqs dense from 1"
    );
}

/// The seed is reproduction element #3: changing it may reorder within-wave commits
/// (merge order), and receipts must reflect whatever order actually happened — but
/// plan_hash stays fixed (elements #1/#2: plan bytes).
#[test]
fn s1_plan_hash_independent_of_seed() {
    let plan = parse_plan(PLAN).expect("plan compiles");
    let engine = DeterministicEngine;
    let g1 = engine.compile(&plan, 1).expect("compile");
    let g2 = engine.compile(&plan, 999).expect("compile");
    assert_eq!(g1.plan_hash, g2.plan_hash);
    assert_eq!(canonical_sha256(&plan), g1.plan_hash);
}

/// Snapshot round-trip: the compiled graph (waves + PRNG state + commit history)
/// survives canonical serialization — the persistence seam replay (WP-P1e) relies on.
#[test]
fn s1_compiled_graph_snapshot_roundtrips() {
    let plan = parse_plan(PLAN).expect("plan compiles");
    let engine = DeterministicEngine;
    let mut graph = engine.compile(&plan, 7).expect("compile");

    // Partial progress, then snapshot, then finish both the original and the restore.
    let wave = engine.next_wave(&graph).expect("wave");
    engine.commit(&mut graph, output(&wave.nodes[0], 1));

    let bytes = canonical_bytes(&graph);
    let mut restored: hesmos_orchestrator::CompiledGraph =
        serde_json::from_slice(&bytes).expect("snapshot parses");

    assert_eq!(canonical_bytes(&graph), canonical_bytes(&restored));
    // Continuing both must stay in lockstep (same receipts from here on).
    let mut receipts_original = Vec::new();
    let mut n = 1u128;
    while let Some(w) = engine.next_wave(&graph) {
        for node in &w.nodes {
            n += 1;
            receipts_original.push(engine.commit(&mut graph, output(node, n)).event_hash);
        }
    }
    let mut receipts_restored = Vec::new();
    let mut n = 1u128;
    while let Some(w) = engine.next_wave(&restored) {
        for node in &w.nodes {
            n += 1;
            receipts_restored.push(engine.commit(&mut restored, output(node, n)).event_hash);
        }
    }
    assert_eq!(receipts_original, receipts_restored);
}

/// PRNG import seam: a fork that continues from the same snapshot state produces the
/// same downstream draws (D4 — snapshot carries PRNG state, not the seed alone).
#[test]
fn s1_prng_state_continues_stream() {
    let mut a = SplitMix64::new(11);
    a.next_u64();
    let snap = canonical_bytes(&a);
    let mut b: SplitMix64 = serde_json::from_slice(&snap).expect("parse");
    assert_eq!(a.next_u64(), b.next_u64());
}
