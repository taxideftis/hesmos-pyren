//! TRAIT-2 GraphEngine — compile / next_wave / commit, the deterministic heart.
//!
//! Determinism contract (TRAIT-2 invariants, enforced structurally here):
//! - Same (plan, seed) → same plan_hash, same wave list, same wave-internal order, and
//!   the same legal commit sequence. No wall clock, no OS entropy, no HashMap iteration
//!   anywhere in this module (BTree only).
//! - `commit` order IS the wave's node order: parallel nodes may finish in any order,
//!   but their commits are applied in the seeded order — "커밋 순서 고정". A commit
//!   that skips ahead violates the invariant and crashes (the same policy as core's
//!   session transitions: illegal states abort, never "sort of work").
//! - `next_wave`/`commit` never read node *output* — routing consequences of outputs
//!   belong to the router node path (WP-P1c), not to scheduling.
//!
//! The engine does NOT emit events. `plan.compiled` (plan_hash, node_count) and the
//! commit events are the runner's job through the PORT-1 sink (P6 single funnel); the
//! engine only exposes the values. [`CommitReceipt::event_hash`] is the commit *proof*
//! (content hash of the receipt input), which the WAL (WP-P1e) stores with the
//! checkpoint — it is not a trace-chain hash.

use serde::{Deserialize, Serialize};

use hesmos_core::{
    CommitSeq, CompileError, Confidence, Envelope, NodeId, Plan, Sha256Hex, WaveIndex,
    canonical_sha256,
};

use crate::compile::check_pattern;
use crate::prng::SplitMix64;
use crate::waves::{Wave, build_waves};

/// TRAIT-2 (James's design — signature is frozen).
pub trait GraphEngine {
    /// Plan → DAG → ordered waves. Pure: on `Err` no state exists anywhere (exit 3
    /// path, US-02 AC1).
    fn compile(&self, plan: &Plan, seed: u64) -> Result<CompiledGraph, CompileError>;
    /// The active wave, or `None` when every wave is fully committed.
    fn next_wave(&self, g: &CompiledGraph) -> Option<Wave>;
    /// Applies a post-gate output in fixed wave order; returns the WAL receipt.
    fn commit(&self, g: &mut CompiledGraph, out: StageOutput) -> CommitReceipt;
}

/// Stateless marker impl — all state lives in [`CompiledGraph`] (snapshotable, so the
/// runner can persist/reload mid-session and replay can rebuild from it).
#[derive(Debug, Clone, Copy, Default)]
pub struct DeterministicEngine;

/// Node output handed to [`GraphEngine::commit`] (TRAIT-2 shape).
#[derive(Debug, Clone, PartialEq)]
pub struct StageOutput {
    pub node: NodeId,
    pub envelope: Envelope,
    pub confidence: Confidence,
}

/// WAL commit proof (TRAIT-2 shape). `commit_seq` is 1-based and dense within a
/// session; `event_hash` is `sha256(canonical(seq, node, envelope, confidence))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitReceipt {
    pub commit_seq: CommitSeq,
    pub event_hash: Sha256Hex,
}

/// One applied commit, as recorded in the graph snapshot. The WAL (WP-P1e) mirrors
/// these into the checkpoint; `event_hash` is the commit proof, not a chain hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub seq: CommitSeq,
    pub node: NodeId,
    pub event_hash: Sha256Hex,
}

/// The compiled session graph. Serializing this snapshot (waves + PRNG state + commit
/// history) is what replay restores — D4 note from WP-P0b: the PRNG state must be part
/// of the snapshot or forked sessions would re-shuffle from scratch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledGraph {
    /// `sha256(canonical(plan))` — reproduction element #2 (recorded on plan.compiled).
    pub plan_hash: Sha256Hex,
    /// Stage count (recorded on plan.compiled next to plan_hash — AC3).
    pub node_count: usize,
    /// Seed (reproduction element #3, recorded on session.open by the runner).
    seed: u64,
    /// Fixed-order waves: outer order topological, inner order seed-derived.
    waves: Vec<Wave>,
    /// Index into `waves` of the wave currently being committed.
    cursor: usize,
    /// How many nodes of the current wave have been committed (they commit in order).
    wave_commit_pos: usize,
    /// PRNG state — advanced exactly once per node during compile; carried in the
    /// snapshot so forked sessions continue the stream instead of restarting it.
    prng: SplitMix64,
    /// Commit history, dense from CommitSeq(1).
    commits: Vec<CommitRecord>,
}

/// Canonical hash input of a receipt — field order IS the hash order.
#[derive(Serialize)]
struct ReceiptInput<'a> {
    commit_seq: u64,
    node: &'a NodeId,
    envelope: &'a Envelope,
    confidence: f32,
}

impl GraphEngine for DeterministicEngine {
    fn compile(&self, plan: &Plan, seed: u64) -> Result<CompiledGraph, CompileError> {
        // CE-05 defense for programmatically built plans (parse_plan already rejects
        // absent/blank tasks — this catches direct Plan construction).
        if plan.task.trim().is_empty() {
            return Err(CompileError::MissingGoalOriginal);
        }

        // CE-08 — duplicate ids cannot address waves.
        let mut nodes = std::collections::BTreeSet::new();
        for stage in &plan.stages {
            if !nodes.insert(stage.id.clone()) {
                return Err(CompileError::DuplicateNodeId {
                    node: stage.id.as_str().to_string(),
                });
            }
        }

        // CE-02 defense — edges must reference compiled nodes.
        let mut succs: std::collections::BTreeMap<NodeId, std::collections::BTreeSet<NodeId>> =
            std::collections::BTreeMap::new();
        for n in &nodes {
            succs.entry(n.clone()).or_default();
        }
        for e in &plan.edges {
            if !nodes.contains(&e.from) || !nodes.contains(&e.to) {
                let refs: Vec<String> = [e.from.as_str(), e.to.as_str()]
                    .into_iter()
                    .filter(|n| !nodes.contains(&NodeId::new(*n)))
                    .map(String::from)
                    .collect();
                return Err(CompileError::UnknownNodeRef {
                    node: "<edges>".to_string(),
                    refs,
                });
            }
            succs
                .get_mut(&e.from)
                .expect("endpoint checked")
                .insert(e.to.clone());
        }

        // CE-07 defense — declared pattern must fit the edge shape (parse_plan checked
        // its own fold; this covers Plan values built by hand).
        check_pattern(&plan.pattern, &plan.stages, &plan.edges)?;

        // CE-03 — cycles surface here as wave-construction failure, with the path.
        let mut prng = SplitMix64::new(seed);
        let waves = build_waves(&nodes, &succs, &mut prng)?;

        Ok(CompiledGraph {
            plan_hash: canonical_sha256(plan),
            node_count: plan.stages.len(),
            seed,
            waves,
            cursor: 0,
            wave_commit_pos: 0,
            prng,
            commits: Vec::new(),
        })
    }

    fn next_wave(&self, g: &CompiledGraph) -> Option<Wave> {
        g.waves.get(g.cursor).cloned()
    }

    fn commit(&self, g: &mut CompiledGraph, out: StageOutput) -> CommitReceipt {
        let wave = g
            .waves
            .get(g.cursor)
            .expect("commit with no active wave — runner committed past the last wave");
        let expected = &wave.nodes[g.wave_commit_pos];
        assert_eq!(
            &out.node, expected,
            "commit order violation: wave order is fixed (expected {expected}, got {})",
            out.node
        );

        let seq = CommitSeq(g.commits.len() as u64 + 1);
        let input = ReceiptInput {
            commit_seq: seq.0,
            node: &out.node,
            envelope: &out.envelope,
            confidence: out.confidence.get(),
        };
        let event_hash = canonical_sha256(&input);

        g.commits.push(CommitRecord {
            seq,
            node: out.node,
            event_hash: event_hash.clone(),
        });
        g.wave_commit_pos += 1;
        if g.wave_commit_pos == wave.nodes.len() {
            g.cursor += 1;
            g.wave_commit_pos = 0;
        }
        CommitReceipt {
            commit_seq: seq,
            event_hash,
        }
    }
}

impl CompiledGraph {
    /// Seed value (reproduction element #3) — read by the runner for session.open.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Immutable wave list view (runner emits wave scheduling / trace display).
    pub fn waves(&self) -> &[Wave] {
        &self.waves
    }

    /// Current wave index (cursor).
    pub fn cursor(&self) -> WaveIndex {
        WaveIndex(self.cursor as u32)
    }

    /// Commit receipts so far, in commit order.
    pub fn commits(&self) -> impl Iterator<Item = &CommitRecord> {
        self.commits.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::parse_plan;
    use hesmos_core::{CorrelationId, EnvelopeId, EnvelopeKind, Payload, SchemaId, Taint};

    const PLAN_YAML: &str = r#"
name: repro
task: draft a report
pattern: graph
stages:
  - id: research
    profile: {role: researcher, model: glm-5.3-flash, tools: [web.search]}
    input_schema: research.v1
    done_criteria: {items: [notes]}
  - id: draft
    profile: {role: writer, model: glm-5.3-flash}
    input_schema: draft.v1
    done_criteria: {items: [draft]}
  - id: review
    profile: {role: reviewer, model: glm-5.3-flash}
    input_schema: review.v1
    done_criteria: {items: [reviewed]}
flow: "research -> draft; draft -> review"
"#;

    fn plan() -> Plan {
        parse_plan(PLAN_YAML).expect("plan compiles")
    }

    fn output(node: &str, n: u128) -> StageOutput {
        StageOutput {
            node: NodeId::new(node),
            envelope: hesmos_core::Envelope {
                id: EnvelopeId::from_u128(n),
                from: NodeId::new(node),
                to: NodeId::new("next"),
                kind: EnvelopeKind::Result,
                payload: Payload {
                    schema_id: SchemaId::new("out.v1"),
                    json: serde_json::json!({ "n": n }),
                },
                correlation_id: CorrelationId::from_u128(n),
                taint: Taint::Clean,
            },
            confidence: Confidence::new(0.9).expect("in range"),
        }
    }

    /// T3 core: same (plan, seed) → byte-identical compiled graph (waves + hash).
    #[test]
    fn same_plan_same_seed_same_graph() {
        let engine = DeterministicEngine;
        let g1 = engine.compile(&plan(), 42).expect("compile");
        let g2 = engine.compile(&plan(), 42).expect("compile");
        assert_eq!(g1.plan_hash, g2.plan_hash);
        assert_eq!(
            hesmos_core::canonical_bytes(&g1.waves().to_vec()),
            hesmos_core::canonical_bytes(&g2.waves().to_vec())
        );
    }

    /// AC3 surface: the runner reads plan_hash + node_count for plan.compiled.
    #[test]
    fn graph_exposes_plan_compiled_attrs() {
        let engine = DeterministicEngine;
        let g = engine.compile(&plan(), 42).expect("compile");
        assert_eq!(g.node_count, 3);
        assert_eq!(
            g.waves().len(),
            3,
            "research -> draft -> review is 3 levels"
        );
        assert_eq!(g.seed(), 42);
    }

    /// Commit lifecycle: waves open in order, commits are dense 1..n, wave closes then
    /// the next opens, and finally next_wave is None.
    #[test]
    fn wave_lifecycle_and_dense_commit_seqs() {
        let engine = DeterministicEngine;
        let mut g = engine.compile(&plan(), 42).expect("compile");

        let mut seq: u64 = 0;
        while let Some(wave) = engine.next_wave(&g) {
            for node in &wave.nodes {
                seq += 1;
                let receipt = engine.commit(&mut g, output(node.as_str(), u128::from(seq)));
                assert_eq!(receipt.commit_seq.0, seq, "commit seq dense from 1");
            }
        }
        assert!(engine.next_wave(&g).is_none());
        assert_eq!(seq, 3);
        assert_eq!(g.commits().count(), 3);
    }

    /// Commit order is the wave order — skipping ahead is an invariant violation and
    /// must crash (never silently reorder).
    #[test]
    #[should_panic(expected = "commit order violation")]
    fn out_of_order_commit_panics() {
        let engine = DeterministicEngine;
        let mut g = engine.compile(&plan(), 42).expect("compile");
        // Wave 0 is [research]; committing draft first violates the fixed order.
        engine.commit(&mut g, output("draft", 1));
    }

    /// Same outputs committed in the same (fixed) order produce identical receipts —
    /// the structural S1 primitive (byte-level trace equality is the runner's, WP-P1e).
    #[test]
    fn receipts_deterministic_for_same_outputs() {
        let engine = DeterministicEngine;
        let mut g1 = engine.compile(&plan(), 42).expect("compile");
        let mut g2 = engine.compile(&plan(), 42).expect("compile");

        let mut receipts1 = Vec::new();
        let mut receipts2 = Vec::new();
        let mut n = 0u128;
        while let Some(w) = engine.next_wave(&g1) {
            for node in &w.nodes {
                n += 1;
                receipts1.push(engine.commit(&mut g1, output(node.as_str(), n)));
                receipts2.push(engine.commit(&mut g2, output(node.as_str(), n)));
            }
        }
        assert_eq!(receipts1, receipts2, "same inputs → same receipts");
        // And the envelope content moves the hash (a different output → other hash).
        let mut g3 = engine.compile(&plan(), 42).expect("compile");
        let w = engine.next_wave(&g3).expect("wave");
        let r_other = engine.commit(&mut g3, output(w.nodes[0].as_str(), 999));
        assert_ne!(receipts1[0].event_hash, r_other.event_hash);
    }
}
