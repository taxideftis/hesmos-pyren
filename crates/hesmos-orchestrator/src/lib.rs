//! hesmos-orchestrator — the deterministic heart (code-structure §1).
//!
//! Owns compile (Plan→DAG, CE-01..09, flow DSL), wave scheduling with fixed commit
//! order, the seeded PRNG, routing with loop guards, the SQLite WAL, the session runner
//! and the bathos CLI bridge. It *decides order*; judgment belongs to hesmos-guard and
//! metering to hesmos-budget — cross-component collaboration happens only through core
//! types and the runner's conducting (code-structure §2, D-2).
//!
//! P1e status: compile / waves / prng / engine / router / wal / runner / bathos
//! platform adapter live. Remaining in this WP: the composition root (hesmos binary).

mod compile;
mod engine;
mod platform;
mod prng;
mod router;
mod runner;
mod wal;
mod waves;

// D-7: the public surface is the re-export list below; the modules themselves are private.
pub use compile::parse_plan;
pub use engine::{CommitReceipt, CompiledGraph, DeterministicEngine, GraphEngine, StageOutput};
pub use platform::BathosCli;
pub use prng::SplitMix64;
pub use router::{
    ContractValidator, HandoffRouter, LoopGuardRouter, LoopGuardState, LoopGuardVerdict,
};
pub use runner::{
    BudgetConductor, EchoExecutor, EchoMode, ExecutionReport, ForkSource, GateConductor, GateRun,
    MeterKind, RunOutcome, Runner, RunnerConfig, RunnerError, RunnerPhase, SpendVerdict,
    StageExecutor, StageFailure, StageRequest, derive_fork_session_id,
};
pub use wal::{
    SessionRow, SessionWal, StoredCommit, WalError, parse_fork_of, parse_run_id, parse_session_id,
    parse_team_id, session_row_of,
};
pub use waves::Wave;
