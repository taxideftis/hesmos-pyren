//! hesmos-orchestrator — the deterministic heart (code-structure §1).
//!
//! Owns compile (Plan→DAG, CE-01..09, flow DSL), wave scheduling with fixed commit
//! order, the seeded PRNG, routing with loop guards, the SQLite WAL, the session runner
//! and the bathos CLI bridge. It *decides order*; judgment belongs to hesmos-guard and
//! metering to hesmos-budget — cross-component collaboration happens only through core
//! types and the runner's conducting (code-structure §2, D-2).
//!
//! P1a status: compile / waves / prng / engine live. Later: router (WP-P1c), wal +
//! runner + platform bridge (WP-P1e).

mod compile;
mod engine;
mod prng;
mod waves;

// D-7: the public surface is the re-export list below; the modules themselves are private.
pub use compile::parse_plan;
pub use engine::{CommitReceipt, CompiledGraph, DeterministicEngine, GraphEngine, StageOutput};
pub use prng::SplitMix64;
pub use waves::Wave;
