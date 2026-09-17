//! Wave decomposition with fixed commit order (표 9 / SS-04).
//!
//! A wave is one topological level of the plan DAG. Two orderings are fixed here:
//! 1. wave order — Kahn levels, deterministic regardless of input stage order;
//! 2. order *within* a wave — seeded PRNG draws assigned in sorted-node order, then a
//!    final sort by `(draw, node_id)`. The node-id tiebreak keeps the result stable even
//!    if a draw collided, and the draw keeps Parallel merge order seed-dependent (표 9:
//!    "통합 순서 시드 고정") instead of arrival-ordered (a P1 violation).
//!
//! Cycle detection lives here too: Kahn leftovers are a cycle, and the offending path
//! is extracted for CE-03's `cycle` field (US-02 AC1: errors carry locations).

use std::collections::{BTreeMap, BTreeSet};

use hesmos_core::{CompileError, NodeId, WaveIndex};

use crate::prng::SplitMix64;

/// One topological wave. `nodes` order IS the commit order (fixed, seed-derived).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Wave {
    pub index: WaveIndex,
    pub nodes: Vec<NodeId>,
}

/// Adjacency: node → its direct predecessors / successors.
pub(crate) type Adjacency = BTreeMap<NodeId, BTreeSet<NodeId>>;

// DFS coloring states for cycle-path extraction.
const IN_PROGRESS: u8 = 1;
const DONE: u8 = 2;

/// Builds the ordered wave list for `nodes` under `edges` (from → to).
///
/// `prng` is advanced by exactly one draw per node, in sorted-node order per wave, so
/// the same (graph, prng state) always yields the same ordering.
pub(crate) fn build_waves(
    nodes: &BTreeSet<NodeId>,
    succs: &Adjacency,
    prng: &mut SplitMix64,
) -> Result<Vec<Wave>, CompileError> {
    let mut preds: Adjacency = BTreeMap::new();
    for n in nodes {
        preds.entry(n.clone()).or_default();
    }
    for (from, tos) in succs {
        for to in tos {
            preds
                .get_mut(to)
                .expect("edge endpoint validated upstream")
                .insert(from.clone());
        }
    }

    let mut remaining: BTreeSet<NodeId> = nodes.clone();
    let mut done: BTreeSet<NodeId> = BTreeSet::new();
    let mut waves = Vec::new();

    while !remaining.is_empty() {
        // Level = every remaining node whose predecessors are all done.
        let level: Vec<NodeId> = remaining
            .iter()
            .filter(|n| preds[n].is_subset(&done))
            .cloned()
            .collect();
        if level.is_empty() {
            return Err(CompileError::CyclicDependency {
                cycle: extract_cycle(nodes, succs),
            });
        }

        // Fixed within-wave order: seed draws keyed by sorted node id, then sort.
        let mut keyed: Vec<(u64, NodeId)> =
            level.iter().map(|n| (prng.next_u64(), n.clone())).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let ordered: Vec<NodeId> = keyed.into_iter().map(|(_, n)| n).collect();

        for n in &level {
            remaining.remove(n);
            done.insert(n.clone());
        }
        let index = WaveIndex(waves.len() as u32);
        waves.push(Wave {
            index,
            nodes: ordered,
        });
    }
    Ok(waves)
}

/// Walks the leftover graph for one concrete cycle path (CE-03 reports it verbatim).
fn extract_cycle<'a>(nodes: &'a BTreeSet<NodeId>, succs: &'a Adjacency) -> Vec<String> {
    let mut color: BTreeMap<&NodeId, u8> = BTreeMap::new();
    let mut path: Vec<&NodeId> = Vec::new();

    for start in nodes {
        if dfs(start, succs, &mut color, &mut path) {
            // Rotate the path to begin at the cycle entry for a readable report.
            let cycle: Vec<String> = path.iter().map(|n| n.as_str().to_string()).collect();
            return cycle;
        }
        path.clear();
    }
    vec!["<cycle detected but path extraction failed>".to_string()]
}

fn dfs<'a>(
    node: &'a NodeId,
    succs: &'a Adjacency,
    color: &mut BTreeMap<&'a NodeId, u8>,
    path: &mut Vec<&'a NodeId>,
) -> bool {
    match color.get(node) {
        Some(&DONE) => false,
        Some(&IN_PROGRESS) => {
            // Found the cycle: trim the path so it starts at this node.
            let pos = path.iter().position(|n| *n == node).unwrap_or(0);
            path.drain(..pos);
            true
        }
        // Only the two states above are ever inserted.
        Some(&other) => unreachable!("unknown DFS color {other}"),
        None => {
            color.insert(node, IN_PROGRESS);
            path.push(node);
            for next in succs.get(node).into_iter().flatten() {
                if dfs(next, succs, color, path) {
                    return true;
                }
            }
            color.insert(node, DONE);
            path.pop();
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> BTreeSet<NodeId> {
        names.iter().map(|n| NodeId::new(*n)).collect()
    }

    fn edge(from: &str, to: &str) -> (NodeId, NodeId) {
        (NodeId::new(from), NodeId::new(to))
    }

    fn adj(pairs: &[(NodeId, NodeId)]) -> Adjacency {
        let mut m: Adjacency = BTreeMap::new();
        for (f, t) in pairs {
            m.entry(f.clone()).or_default().insert(t.clone());
        }
        m
    }

    #[test]
    fn levels_are_topological_and_order_fixed() {
        let nodes = ids(&["a", "b", "c", "d"]);
        let pairs = vec![edge("a", "b"), edge("b", "c"), edge("a", "c")];
        let succs = adj(&pairs);
        let mut prng = SplitMix64::new(42);
        let waves = build_waves(&nodes, &succs, &mut prng).expect("dag");
        assert_eq!(waves.len(), 3);
        // `d` is edgeless, so it is a root of wave 0 next to `a`; wave-internal order
        // is seed-derived, so compare as sorted sets.
        let wave0: BTreeSet<&NodeId> = waves[0].nodes.iter().collect();
        assert_eq!(
            wave0,
            BTreeSet::from([&NodeId::new("a"), &NodeId::new("d")])
        );
        assert_eq!(waves[1].nodes, vec![NodeId::new("b")]);
        assert_eq!(waves[2].nodes, vec![NodeId::new("c")]);
        // Every successor appears strictly later than its predecessor.
        for w in &waves {
            for n in &w.nodes {
                for to in succs.get(n).into_iter().flatten() {
                    let later = waves
                        .iter()
                        .skip(usize::try_from(w.index.0).expect("fits") + 1)
                        .any(|lw| lw.nodes.contains(to));
                    assert!(later, "{n} -> {to} violates wave order");
                }
            }
        }
    }

    #[test]
    fn cycle_is_reported_with_path() {
        let nodes = ids(&["a", "b", "c"]);
        let pairs = vec![edge("a", "b"), edge("b", "c"), edge("c", "a")];
        let mut prng = SplitMix64::new(42);
        let err = build_waves(&nodes, &adj(&pairs), &mut prng).expect_err("cycle");
        match err {
            CompileError::CyclicDependency { cycle } => {
                assert_eq!(cycle, vec!["a", "b", "c"], "cycle path verbatim");
                assert_eq!(CompileError::CyclicDependency { cycle }.code(), "CE-03");
            }
            other => panic!("expected CE-03, got {other:?}"),
        }
    }

    /// Same (graph, seed) → byte-identical wave order; the T3 primitive.
    #[test]
    fn same_seed_same_wave_order() {
        let nodes = ids(&["n1", "n2", "n3", "n4", "n5"]);
        let pairs: Vec<(NodeId, NodeId)> = vec![edge("n1", "n3"), edge("n2", "n3")];
        let mut p1 = SplitMix64::new(42);
        let mut p2 = SplitMix64::new(42);
        let w1 = build_waves(&nodes, &adj(&pairs), &mut p1).expect("dag");
        let w2 = build_waves(&nodes, &adj(&pairs), &mut p2).expect("dag");
        assert_eq!(
            hesmos_core::canonical_bytes(&w1),
            hesmos_core::canonical_bytes(&w2)
        );
    }
}
