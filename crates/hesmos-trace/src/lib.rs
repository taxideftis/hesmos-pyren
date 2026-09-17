//! hesmos-trace — the recorder, nothing more (code-structure §1/§3).
//!
//! Owns the PORT-1 default implementation (jsonl hash-chain EventLog), tamper
//! verification, trace queries/projections, replay/fork and the seal that submits the
//! chain head to bathos audit via PORT-2. Forbidden here: judgments and scheduling —
//! the trace layer never intervenes in execution.
//!
//! Modules: `log` (WP-P0b) · `seal`/`replay` (WP-P1e). Arriving later: query and the
//! opt-in OTel sink (WP-P3a).

mod log;
mod replay;
mod seal;

// D-7: the public surface is the re-export list below; the modules themselves are private.
pub use log::{EventLog, GENESIS_PREV_HASH, LoadOutcome, TraceError, load_path};
pub use replay::{ReproductionComparison, chain_head, compare_reproduction};
pub use seal::{SealError, SealOutcome, seal};

/// Compile-time proof of the D-2 dependency direction: the crate reaches the chain-hash
/// primitive it links events with.
#[cfg(test)]
mod p0a_wiring {
    #[test]
    fn chain_hash_primitive_is_reachable() {
        let attrs = hesmos_core::EventAttrs::new().set("k", "v");
        let hash = hesmos_core::chain_hash(
            &hesmos_core::Sha256Hex::parse("0".repeat(64)).expect("genesis hex"),
            0,
            hesmos_core::EventKind::SessionOpen,
            None,
            &attrs,
        );
        assert_ne!(hash.as_str(), "0".repeat(64));
    }
}
