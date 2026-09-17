//! The gateway's consumption surface over the core.
//!
//! This trait is the **only** path from HTTP to session data (SS-24 rule 1:
//! the gateway is a thin adapter; contact points are stateless). Today it is
//! implemented by [`crate::stub::StubCore`]; once `hesmos-core` lands, a small
//! adapter implements it against the core's public API (SessionHandle queries,
//! trace re-composition, budget aggregation, audit pass-through via PORT-2).
//!
//! Wire types here are the HTTP-1~5 contract shapes. They deliberately mirror
//! the *contract*, not the core structs — mapping core → wire happens in the
//! adapter, keeping `api-contracts.md` the single source for field names.

use std::collections::BTreeMap;

use serde::Serialize;
use tokio::sync::broadcast;

/// HTTP-2 hard cap — `limit<=500`; unbounded event queries are rejected.
pub const EVENTS_LIMIT_MAX: u16 = 500;

/// Session state vocabulary — TYPE-5 `SessionState`, 7 fixed variants. Wire
/// spelling is the contract's own (PascalCase, exactly what core's serde
/// emits) so a real adapter can pass `SessionHandle.state` through untouched
/// (SS-24: thin adapter, no reinterpretation). The uppercase display word
/// (`✓ COMPLETED`, tokens §2.1) is a UI-side transform, applied in `chip`.
/// (Cancelled is an unreachable reserved variant per W3-부4; still accepted on
/// the wire so the enum stays exhaustive.)
pub const SESSION_STATES: [&str; 7] = [
    "Init",
    "Running",
    "Suspended",
    "Halted",
    "Completed",
    "Cancelled",
    "Failed",
];

/// HTTP-1 row: exactly `{session_id, state, team_id?, budget_spent, chain_head?}`.
/// Optional fields are omitted when absent (the `?` in the contract).
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// Spent tokens (absolute). HTTP-1 carries no limit, so D1 shows the
    /// absolute value — a usage bar would need a denominator the contract
    /// does not provide (ui-spec §9.2, ux-flow-map J-1).
    pub budget_spent: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_head: Option<String>,
}

/// Budget envelope projection (TYPE-6 `BudgetEnvelope`) for HTTP-2 snapshots.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_max_tokens: Option<u64>,
    pub agent_max_tokens: BTreeMap<String, u64>,
    pub warn_pct: u8,
    pub suspend_pct: u8,
}

/// HTTP-2 snapshot body — the SessionHandle projection (TYPE-5 fields).
#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub run_id: String,
    pub seed: u64,
    pub plan_hash: String,
    pub budget: BudgetWire,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// Replay lineage, kept in the contract's own shape `(SessionId,
    /// CommitSeq)` — serializes as `[ulid, commit_seq]`, so the adapter can
    /// pass the core tuple through untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<(String, u64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_head: Option<String>,
}

/// HTTP-2/WS event body (TYPE-3 `TraceEvent` projection).
///
/// `kind` uses the fixed machine vocabulary (`session.open`, `gate.pass`, … —
/// tokens §2.4) so D2 renders the name with zero translation (ui-spec §1.2).
///
/// `commit_seq` is a display-only annotation for the `→ commit #N` marker
/// (P-10, W3-3: N = CommitSeq). It is derived from core WAL receipts — not a
/// TYPE-3 field — and is omitted when the event is not a commit point.
/// `ts` is stored observation time, unix millis (`u64`, matching core's
/// `TraceEvent.ts` 1:1), display-only: it is excluded from the chain hash
/// input by contract (TYPE-3 invariant 1 / ADR-0006).
#[derive(Debug, Clone, Serialize)]
pub struct TraceEventWire {
    pub seq: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Kind-specific attributes; BTreeMap keeps serialization order stable.
    pub attrs: BTreeMap<String, serde_json::Value>,
    pub prev_hash: String,
    pub hash: String,
    pub ts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_seq: Option<u64>,
}

/// HTTP-4 body — the 3-unit aggregate, identical output to `hesmos budget
/// --team` (CLI-4 / §6.3). Sub-shapes follow the CLI row vocabulary:
/// session rows (spent/limit/pct/state/warn), team summary, agent shares.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetReport {
    pub session: Vec<BudgetSessionRow>,
    pub team: TeamSummary,
    pub agent: Vec<AgentShare>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetSessionRow {
    pub session_id: String,
    pub spent: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct: Option<u8>,
    pub state: String,
    /// True when a `budget.event level=warn` was observed for this session.
    pub warn: bool,
    /// Threshold in force (charter §6 default 80) — the `! N% 경고` marker.
    pub warn_pct: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct TeamSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    pub spent: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct: Option<u8>,
    pub warn_count: u32,
    pub suspend_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentShare {
    pub agent_role: String,
    pub spent: u64,
    pub pct: u8,
}

/// HTTP-5 body — exactly `{chain_head_hash, bathos_audit_verified}`.
/// The bool is a pass-through of the core's audit verify (PORT-2) result;
/// the gateway never verifies chains itself.
#[derive(Debug, Clone, Serialize)]
pub struct AuditStatus {
    pub chain_head_hash: String,
    pub bathos_audit_verified: bool,
}

/// Core-side failure, mapped to the HTTP error body by [`crate::error::ApiError`].
#[derive(Debug)]
pub enum CoreError {
    /// Session (or sealed trace) does not exist → 404.
    NotFound(String),
    /// Malformed query / unsupported combination → 400 (USAGE-ARGS).
    Usage(String),
    /// Action impossible in the current core state → 409 (USAGE-STATE).
    Conflict(String),
    /// Core query failed → 500 (Platform passthrough).
    Internal(String),
}

/// The stateless query surface every route goes through.
pub trait CorePort: Send + Sync {
    /// All sessions, oldest first (D1 renders newest first by reversing).
    fn list_sessions(&self) -> Result<Vec<SessionSummary>, CoreError>;
    /// SessionHandle snapshot, `None` when the id is unknown.
    fn session(&self, id: &str) -> Option<SessionSnapshot>;
    /// Events ascending, at most `limit` (≤ [`EVENTS_LIMIT_MAX`]). `after` is
    /// an exclusive cursor (`seq > after`); `None` means the very beginning
    /// including seq 0 — the distinction matters because `?after=0` must skip
    /// seq 0 while the initial load must include it.
    fn events(
        &self,
        id: &str,
        after: Option<u64>,
        limit: u16,
    ) -> Result<Vec<TraceEventWire>, CoreError>;
    /// 3-unit budget aggregate, optionally scoped to one team.
    fn budgets(&self, team_id: Option<&str>) -> Result<BudgetReport, CoreError>;
    /// Audit verify pass-through: `Ok(None)` = session unknown or not sealed.
    fn audit(&self, id: &str) -> Result<Option<AuditStatus>, CoreError>;
    /// Live fan-out subscription (HTTP-3). The receiver only sees events
    /// emitted **after** subscription — no backfill; catch-up is HTTP-2
    /// `?after=<seq>` (always safe: the trace is an event-sourced log, A-N5).
    fn subscribe(&self, id: &str) -> Result<broadcast::Receiver<TraceEventWire>, CoreError>;
}

pub type EventsPage = Vec<TraceEventWire>;
