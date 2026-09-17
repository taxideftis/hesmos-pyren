//! hesmos-core — schema crate and single source of truth for TYPE-1..7 contracts.
//!
//! This crate owns types, traits and canonical serialization only: no I/O, no process
//! calls, no state, no orchestration logic (code-structure §3). Every other crate consumes
//! these schemas; re-defining a core type elsewhere is a boundary violation (task-graph
//! boundary rule 3). Dependencies point one way: core depends on nothing internal (D-1).
//!
//! Determinism rules baked into the schemas here (project-context §2):
//! - serde serializes structs in field declaration order — the stability source for hashes.
//! - collections are BTree-based; do not enable serde_json's `preserve_order`.
//! - identifiers are newtypes; raw String/u64 never cross a boundary.

mod budget_state;
mod contract;
mod envelope;
mod error;
mod ids;
mod plan;
mod platform;
mod policy;
mod session;
mod sink;
mod trace_event;

// D-7: the public surface is the re-export list below; the modules themselves are private.

pub use budget_state::BudgetState;
pub use contract::{
    ArtifactRef, Assumption, Confidence, ContractBody, ContractJudgment, DoneCriteria,
    FailedApproach, HandoffContract, KnowledgeEntry, PermissionCap, SnapshotSlice,
};
pub use envelope::{Envelope, EnvelopeKind, Payload, Taint, TaintSource};
pub use error::{
    CompileError, ErrorClass, GateVerdict, HesmosError, ReasonCode, RouteDecision, SchemaError,
    code_to_static,
};
pub use ids::{
    AgentRole, CommitSeq, CorrelationId, EnvelopeId, ModelRef, NodeId, RunId, SchemaId, SessionId,
    Sha256Hex, TeamId, ToolName, WaveIndex,
};
pub use plan::{
    AgentProfile, BudgetEnvelope, BudgetSpec, Edge, GateSpec, KnowledgeSpec, NetworkScope,
    PatternKind, Plan, ResourceLimits, Stage, StageSpec,
};
pub use platform::{
    AuditPayload, BathosEngine, GateRecord, GateRecordId, GateReport, ModelReport, PlatformError,
    RawReport, StateReport, VerifyReport, WaveRef, WaveReport,
};
pub use policy::{PolicyOverrides, PolicySet};
pub use session::{SessionHandle, SessionState};
pub use sink::{EventSink, PendingEvent};
pub use trace_event::{
    EventAttrs, EventKind, OPTIONAL_ATTR_KEYS, REQUIRED_ATTRS, TraceEvent, chain_hash,
};

/// Canonical serialization for hashing and golden comparison.
///
/// `serde_json::to_vec` writes struct fields in declaration order and serializes maps in
/// key order (BTreeMap by default) — that, not extra normalization, is what makes these
/// bytes stable across runs. Hash inputs therefore must not contain wall-clock data; the
/// chain hash deliberately excludes `ts` for the same reason (ADR-0006).
pub fn canonical_bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("schema types are infallible to serialize")
}

/// `sha256(canonical(value))` as a validated [`Sha256Hex`].
pub fn canonical_sha256<T: serde::Serialize>(value: &T) -> Sha256Hex {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(canonical_bytes(value));
    let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    Sha256Hex::parse(hex).expect("sha256 hex digest is always 64 lowercase hex chars")
}
