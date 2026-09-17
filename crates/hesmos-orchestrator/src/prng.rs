//! Seeded SplitMix64 — the only randomness source in Hesmos (P1: no global entropy).
//!
//! SplitMix64 is chosen because its 64-bit output passes the usual smoke tests while
//! the whole generator is ~10 lines and trivially snapshotable (`state: u64`), which the
//! CompiledGraph snapshot/replay path (WP-P1e) consumes. Cryptographic strength is not
//! a goal — *reproducibility* is: same seed → same stream, always.
//!
//! `thread_rng`/`OsRng`/`SystemTime` are forbidden anywhere in this workspace; a new
//! entropy source requires a spec revision, not a convenience call.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next raw 64-bit value. In-place state advance (wrapping) — never reads the clock
    /// or OS entropy, so the stream is a pure function of the seed.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..bound` (unbiased via 128-bit widening multiply). `bound`
    /// must be > 0 — a zero bound is a caller bug and panics deliberately.
    pub fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "below(0) is a caller bug");
        (((self.next_u64() as u128) * (bound as u128)) >> 64) as u64
    }

    /// In-place Fisher–Yates. The permutation is a pure function of the slice contents
    /// order and the generator state — this is what fixes Parallel merge order (표 9).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same seed → same stream (the T3 determinism primitive everything else builds on).
    #[test]
    fn same_seed_same_stream() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..16 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(43);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn below_stays_in_range() {
        let mut g = SplitMix64::new(7);
        for _ in 0..1_000 {
            assert!(g.below(5) < 5);
        }
    }

    /// Shuffle is deterministic for a given seed and always a permutation.
    #[test]
    fn shuffle_is_deterministic_permutation() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        let mut xs: Vec<u32> = (0..32).collect();
        let mut ys = xs.clone();
        a.shuffle(&mut xs);
        b.shuffle(&mut ys);
        assert_eq!(xs, ys, "same seed shuffles identically");
        let mut sorted = xs.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..32).collect::<Vec<u32>>(), "still a permutation");
    }

    /// PRNG state round-trips through the snapshot format (CompiledGraph replay needs it).
    #[test]
    fn state_snapshot_roundtrip() {
        let mut g = SplitMix64::new(99);
        g.next_u64();
        g.next_u64();
        let bytes = hesmos_core::canonical_bytes(&g);
        let mut back: SplitMix64 = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(g, back);
        assert_eq!(g.next_u64(), back.next_u64());
    }
}
