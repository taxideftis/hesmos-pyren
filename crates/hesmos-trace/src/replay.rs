//! Replay-side helpers the trace layer owns: reading a stored chain as reproduction
//! evidence and comparing two chains for the S1 claim ("동일 4요소 전체 재생의 trace
//! 해시는 결정적으로 동일").
//!
//! The fork orchestration itself (loading checkpoint.db, minting the fork session,
//! driving the runner) deliberately does NOT live here: checkpoint.db is orchestrator
//! property (data-model-erd §4) and trace depends on core only (D-2). The CLI
//! composition conducts: orchestrator::wal reads the origin, this module verifies the
//! origin chain and judges the reproduction comparison.

use hesmos_core::{Sha256Hex, TraceEvent};

/// The head of a stored chain, or genesis-equivalent `None` for an empty trace.
pub fn chain_head(events: &[TraceEvent]) -> Option<&Sha256Hex> {
    events.last().map(|e| &e.hash)
}

/// The S1 verdict for a reproduction run. Comparison is byte-level on the chain fields
/// (seq·kind·node·attrs·hash — `ts` is excluded by ADR-0006, which is what makes two
/// runs at different wall-clock times comparable at all).
///
/// `heads_equal` alone is sufficient for S1 (the head commits to the whole chain), but
/// the event-count check is kept as a cheap first differential: when counts differ, the
/// divergence point is before the head and the count pair is what the CLI reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReproductionComparison {
    pub heads_equal: bool,
    pub event_count_equal: bool,
}

pub fn compare_reproduction(a: &[TraceEvent], b: &[TraceEvent]) -> ReproductionComparison {
    ReproductionComparison {
        heads_equal: chain_head(a) == chain_head(b),
        event_count_equal: a.len() == b.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::EventAttrs;

    fn event(seq: u64, score: f32, prev: &Sha256Hex) -> TraceEvent {
        let kind = hesmos_core::EventKind::GatePass;
        let attrs = EventAttrs::new()
            .set("gate_id", "rubric.v1")
            .set("score", score);
        let hash = hesmos_core::chain_hash(prev, seq, kind, None, &attrs);
        TraceEvent {
            seq,
            kind,
            node: None,
            prev_hash: prev.clone(),
            hash,
            attrs,
            ts: 0,
        }
    }

    /// The head commits to the whole chain: identical runs compare equal, a changed
    /// score anywhere diverges the head.
    #[test]
    fn comparison_detects_divergence() {
        let genesis = Sha256Hex::parse(crate::log::GENESIS_PREV_HASH).expect("genesis");
        let a = vec![event(0, 0.9, &genesis)];
        let b = vec![event(0, 0.9, &genesis)];
        let same = compare_reproduction(&a, &b);
        assert_eq!(
            same,
            ReproductionComparison {
                heads_equal: true,
                event_count_equal: true
            }
        );

        let c = vec![event(0, 0.8, &genesis)];
        let diff = compare_reproduction(&a, &c);
        assert!(!diff.heads_equal, "different score → different head");
        assert!(diff.event_count_equal);
    }

    #[test]
    fn empty_chain_has_no_head() {
        assert!(chain_head(&[]).is_none());
    }
}
