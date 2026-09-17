//! The session runner (WP-P1e) — S05..S16 of SEQ-01: it CONDUCTS, it never judges,
//! meters, or routes by itself (code-structure §2 "runner가 지휘").
//!
//! **D-2 seam shape.** This crate may depend on core only, while the real gate / budget
//! machinery lives in sibling crates. The runner therefore consumes seam traits phrased
//! purely in core types, plus [`HandoffRouter`] (whose seam exists since WP-P1c):
//!
//! - [`StageExecutor`] — the node work (P1: [`EchoExecutor`]; a real subprocess adapter
//!   arrives with the Python bridge, hesmos-py PY-1..5).
//! - [`GateConductor`] — runs one named gate and leaves its gate.pass/gate.fail event.
//!   The production adapter (composition root) calls `hesmos_guard`'s `instantiate` +
//!   `run_checked_with`; orchestrator unit tests substitute scripted doubles, and the
//!   real chain is proven end-to-end in `tests/integration_rust`.
//! - [`BudgetConductor`] — spend snapshots for pre-gate ctx and metering of finished
//!   calls. The adapter owns `hesmos_budget`'s engine + ledger behind `&self`
//!   (interior mutability — session execution is single-runner-threaded by design).
//!
//! **Boundary sequence per node** (fixed — deviations are runner bugs):
//! node.start → pre-gates (G0 · permission · budget · schema ∪ stage.pre) →
//! execute → llm.call + meter → build output envelope → post-gates (schema ·
//! done_criteria · rubric ∪ stage.post) → route each outgoing edge (handoff.request →
//! decision event) → commit (engine receipt → WAL checkpoint → response cache) →
//! node.stop(done). Only the attempt loop re-runs inside a boundary; node.start /
//! node.stop fire once, retries leave their gate.fail records but no extra stop events.
//!
//! **Suspend lands on a commit boundary.** SS-14 allows checkpoints at commit points
//! only, so budget pressure is enforced at boundary edges: the pre-gate budget gate
//! blocks the next node, and after each commit the metering verdict suspends the
//! session — with the just-written checkpoint as the resume point. Suspending
//! mid-boundary would strand a session with no restore point.
//!
//! **Determinism.** Envelope/correlation ids derive from the node's wave ordinal (never
//! entropy); the executor reports latency verbatim (echo reports 0); `ts` never enters
//! a hash (ADR-0006). Same (plan, seed, executor behavior) → same chain head — the
//! property `trace replay`'s S1 comparison relies on.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use hesmos_core::{
    AgentProfile, AgentRole, BudgetEnvelope, BudgetState, CommitSeq, CompileError, Confidence,
    CorrelationId, Envelope, EnvelopeId, EnvelopeKind, EventAttrs, EventKind, EventSink,
    GateVerdict, HandoffContract, ModelRef, NodeId, Payload, PendingEvent, Plan, PolicySet,
    ReasonCode, RouteDecision, RunId, SchemaId, SessionHandle, SessionId, SessionState, Sha256Hex,
    TeamId, ToolName, WaveIndex, canonical_sha256,
};

use crate::engine::{CommitReceipt, CompiledGraph, DeterministicEngine, GraphEngine, StageOutput};
use crate::router::HandoffRouter;
use crate::wal::SessionWal;

// ---------------------------------------------------------------------------
// Seams (core types only — see module doc)
// ---------------------------------------------------------------------------

/// The invariant every runner-authored handoff contract carries: goal preservation
/// across handoffs. It is not decoration — the guard matrix rejects EMPTY invariants,
/// and this is the one invariant the runner itself enforces (byte-identity with the
/// session task, guard row 1).
pub const HANDOFF_INVARIANT: &str =
    "goal_original stays byte-identical to the session task in every forwarded contract";

/// One node execution request — everything the worker needs, nothing more (TRAIT-1
/// surface). `node_input` is the merged predecessor payload plus the session task.
#[derive(Debug, Clone)]
pub struct StageRequest {
    pub node: NodeId,
    pub wave: WaveIndex,
    /// 0-based execution attempt of this node (bounded-retry loop counter).
    pub attempt: u8,
    pub role: AgentRole,
    pub model: ModelRef,
    pub task: String,
    pub node_input: serde_json::Value,
}

/// A finished node execution (P1: no tool calls — the echo executor uses none; a real
/// adapter reports them through its own tool.call path when WP-P2e lands).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionReport {
    /// Output payload JSON (the committed envelope reuses the stage's `input_schema`).
    pub payload: serde_json::Value,
    pub confidence: f32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Echoed verbatim into the llm.call event (deterministic executors report 0).
    pub latency_ms: u64,
    /// What actually served this turn — the EXECUTOR's self-reported identity
    /// (echo double, FFI bridge, ...), echoed verbatim into the llm.call event.
    /// The runner has no opinion: it never names a provider itself, so a real
    /// adapter's identity reaches the trace un-renamed (ml.md §6d decision 16).
    pub provider: String,
    /// The system prompt hash THIS turn ran with (PY-7 reports per turn). `None` =
    /// the executor has no prompt to report (the W5 echo world) — the cache
    /// sentinel verifies only what a frozen session's turns declare.
    pub prompt_hash: Option<Sha256Hex>,
    /// The external origin this output READ from, when it did (SS-20 rule 1). The
    /// runner marks the output envelope Tainted on this declaration — the only
    /// sanctioned Clean→Tainted path, so an unmarked external envelope is
    /// unrepresentable at the assembly.
    pub external_origin: Option<String>,
}

/// A failed node execution. The runner retries this on the shared bounded budget
/// (SS-10 rule 3 — the LLM-call loop is the SAME knob, no third loop) and halts the
/// session when the budget is spent.
#[derive(Debug, Clone, PartialEq)]
pub struct StageFailure {
    pub reason_code: ReasonCode,
    pub message: String,
}

/// The node-work seam (see module doc).
pub trait StageExecutor {
    fn execute(&self, req: &StageRequest) -> Result<ExecutionReport, StageFailure>;
}

/// Runner-side phase vocabulary — the adapter maps it onto the guard's `GatePhase`
/// (orchestrator cannot name the guard type; the mapping is total and 3 lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerPhase {
    Pre,
    Post,
    G0,
}

/// One gate execution request — the core-type projection of the guard's GateCtx. The
/// adapter fills a real GateCtx from these fields; nothing here names a sibling type.
#[derive(Clone, Copy)]
pub struct GateRun<'a> {
    pub gate_ref: &'a str,
    pub session: &'a SessionHandle,
    /// The runner's PORT-1 sink, lent for this gate. Judgment events MUST flow through
    /// the same sink as every other event (single chain — SS-09 rule 3), so the runner
    /// hands its own down; `SessionHandle` is a data envelope and carries no sink.
    pub sink: &'a dyn EventSink,
    pub node: &'a NodeId,
    pub phase: RunnerPhase,
    pub envelope_in: Option<&'a Envelope>,
    pub envelope_out: Option<&'a Envelope>,
    pub contract: Option<&'a HandoffContract>,
    pub policy: &'a PolicySet,
    pub budget_state: BudgetState,
    pub attempt: u8,
    /// The session task — `contract` gate instantiation needs it (goal_original check).
    pub session_task: &'a str,
    /// The stage's over-delegation floor — `over_delegation` gate instantiation.
    pub spawn_token_floor: u64,
    /// The SS-19 grantor — the FROM node's AgentProfile whose authority bounds the
    /// boundary contract's `permission_cap` (`permission` gate instantiation). `None`
    /// instantiates the profile-less gate, which rejects (AC1 double defense).
    pub grantor_profile: Option<&'a AgentProfile>,
    /// Extra event attributes (WP-P1e escalation records: escalated_to /
    /// escalation_reason) appended to the emitted gate event.
    pub extra: &'a [(&'a str, serde_json::Value)],
}

/// The gate seam (see module doc): run one referenced gate, emit its judgment event,
/// return the verdict. Unknown gate references are a composition invariant — the
/// adapter must panic, not pass.
pub trait GateConductor {
    fn run_gate(&self, req: GateRun<'_>) -> GateVerdict;
}

/// Metered-call kind — seam vocabulary, mapped to the budget crate's kind by the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterKind {
    Llm,
    Tool,
}

/// Threshold verdict — seam vocabulary mirroring the budget engine's judgment (the
/// adapter maps; keeping the shape here avoids an orchestrator→budget type dep, the
/// same resolution the router's ContractValidator seam took for ContractJudgment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpendVerdict {
    Clear,
    /// First crossing of a warn line: `spent` is the scope spend, `remaining` the
    /// suspend-line remainder (`None` = uncapped).
    Warn {
        spent: u64,
        remaining: Option<u64>,
    },
    /// A suspend line is reached — the runner suspends at the next boundary edge.
    Exceeded,
}

/// The metering seam (see module doc). `&self` + adapter-internal mutability, exactly
/// like the router's guard state.
pub trait BudgetConductor {
    /// The immutable spend snapshot for a pre-gate ctx (TRAIT-3 `budget_state`).
    fn snapshot(&self, role: &AgentRole) -> BudgetState;
    /// Meters one finished call and answers with the threshold verdict.
    fn meter(
        &self,
        role: &AgentRole,
        kind: MeterKind,
        tokens_in: u64,
        tokens_out: u64,
    ) -> SpendVerdict;
}

/// The SS-18 cache sentinel seam (WP-P2a core half): verifies ONE turn's reported
/// prompt hash against the session's frozen system-prompt invariant. The judgment
/// lives in the guard (`hesmos_guard::cache`); this seam keeps its type on the
/// composition side (D-2 — the runner never names a sibling crate).
///
/// Contract note (exceptions.md §4 special row): a `Violated` answer means the runner
/// halts IMMEDIATELY — no bounded retry, no second executor call. A deterministic
/// invariant breach cannot be retried away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheVerdict {
    /// Byte-stable prompt (or nothing frozen to verify) — continue.
    Stable,
    /// Hash mismatch / missing report on a frozen session — immediate halt.
    Violated,
}

pub trait CacheConductor {
    /// `reported` is the turn's `ExecutionReport::prompt_hash`.
    fn verify_turn(&self, reported: Option<&Sha256Hex>) -> CacheVerdict;
}

// ---------------------------------------------------------------------------
// EchoExecutor (the P1 executor — ponytail)
// ---------------------------------------------------------------------------

/// ponytail: the P1 "LLM" is a deterministic echo — no network, no model. Tokens are
/// byte counts of the payloads (the same chars ≈ tokens spirit as G0) and confidence is
/// fixed per mode. `Mode` exists so the integration tests can drive every failure path
/// (reject / flaky / retry-then-pass) through the SAME seam the real adapter will use.
/// upgrade trigger: hesmos-py PY-1..5 lands → the composition root swaps this for the
/// subprocess adapter; the runner never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoMode {
    /// Well-formed output, confidence 0.95 — the happy path.
    Echo,
    /// Content-bearing output but confidence 0.3; combined with a raised
    /// `min_confidence` the route rejects it (exit 20 via the router.v1 gate.fail).
    Reject,
    /// Fails the first `n` executions of a node with PROVIDER_FAILURE, then echoes —
    /// proves the bounded provider-retry loop and its exhaustion (exit 12).
    Flaky(usize),
    /// Every node's first attempt returns an empty object (rubric 0.8 → Retry), later
    /// attempts echo properly — the CONCERNS boundary (W3-2 ①) end to end.
    Concerns,
    /// The node "reads" the named external source (WP-P2a): its output is declared
    /// external, so the runner marks the output envelope Tainted and every successor
    /// input inherits the marking (SS-20 transition, end to end).
    External(&'static str),
}

pub struct EchoExecutor {
    pub mode: EchoMode,
    calls: RefCell<BTreeMap<NodeId, usize>>,
}

impl EchoExecutor {
    pub fn new(mode: EchoMode) -> Self {
        Self {
            mode,
            calls: RefCell::new(BTreeMap::new()),
        }
    }
}

impl StageExecutor for EchoExecutor {
    fn execute(&self, req: &StageRequest) -> Result<ExecutionReport, StageFailure> {
        let mut calls = self.calls.borrow_mut();
        let n = *calls.get(&req.node).unwrap_or(&0);
        calls.insert(req.node.clone(), n + 1);
        drop(calls);

        if let EchoMode::Flaky(fail_first) = self.mode
            && n < fail_first
        {
            return Err(StageFailure {
                reason_code: ReasonCode::PROVIDER_FAILURE,
                message: format!("provider unavailable (call {} of node {})", n + 1, req.node),
            });
        }

        let payload = match self.mode {
            EchoMode::Concerns if n == 0 => serde_json::json!({}),
            EchoMode::Reject => serde_json::json!({ "blocked": true, "node": req.node.as_str() }),
            _ => serde_json::json!({
                "node": req.node.as_str(),
                "task": req.task,
                "attempt": req.attempt,
                "input": req.node_input,
            }),
        };
        Ok(ExecutionReport {
            tokens_in: serde_json::to_vec(&req.node_input)
                .map_err(|e| StageFailure {
                    reason_code: ReasonCode::PROVIDER_FAILURE,
                    message: e.to_string(),
                })?
                .len() as u64,
            tokens_out: serde_json::to_vec(&payload)
                .map_err(|e| StageFailure {
                    reason_code: ReasonCode::PROVIDER_FAILURE,
                    message: e.to_string(),
                })?
                .len() as u64,
            confidence: match self.mode {
                EchoMode::Reject => 0.3,
                _ => 0.95,
            },
            latency_ms: 0,
            // The double's identity — the runner used to hardcode this string into
            // llm.call; the report is now the single source (W6 adapters report their
            // own, e.g. the FFI bridge's model_ref backing provider).
            provider: "echo".into(),
            payload,
            // The echo world has no system prompt and reads nothing external — both
            // declarations stay None except the External test mode's origin.
            prompt_hash: None,
            external_origin: match self.mode {
                EchoMode::External(origin) => Some(origin.to_string()),
                _ => None,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Fork source (replay cache, CLI-3)
// ---------------------------------------------------------------------------

/// Replay fork inputs. `at` is the commit point to fork at (the CLI validated it
/// against the WAL). `origin_responses` is the ORIGIN session's `responses/` dir:
/// commits ≤ `at` load their execution from there instead of calling the executor
/// (byte-level response reuse), so a fork of a non-deterministic real executor still
/// reproduces the origin's prefix exactly.
#[derive(Debug, Clone)]
pub struct ForkSource {
    pub origin: SessionId,
    pub at: CommitSeq,
    pub origin_responses: PathBuf,
}

/// Deterministic fork id: u128 truncation of sha256("fork:{origin}#{n}"). A fork mints
/// an id that is NEW relative to the origin (TYPE-5) yet STABLE across re-forks of the
/// same origin at the same point — entropy here is exactly what would break the S1
/// byte-reproduction claim, because session.open carries session_id in the chain.
pub fn derive_fork_session_id(origin: &SessionId, at: CommitSeq) -> SessionId {
    let digest = canonical_sha256(&format!("fork:{}#{}", origin, at.0));
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&digest.as_str()[i * 2..i * 2 + 2], 16).expect("sha256 hex");
    }
    SessionId::from_u128(u128::from_be_bytes(bytes))
}

/// The cached execution of one committed node (`responses/<seq>.json`). The ENVELOPE is
/// stored whole: the fork reuses its bytes verbatim (ids included — they were
/// deterministic at origin time).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedExecution {
    envelope: Envelope,
    confidence: f32,
    tokens_in: u64,
    tokens_out: u64,
    latency_ms: u64,
    /// The ORIGIN turn's serving provider — carried so a fork replay's llm.call
    /// names who actually produced the cached response, not who replayed it.
    provider: String,
}

/// What a boundary obtained as its node output: a live report (build the envelope) or a
/// cached fork execution (envelope verbatim).
enum NodeOutput {
    Live(ExecutionReport),
    Cached(CachedExecution),
}

impl NodeOutput {
    fn tokens_in(&self) -> u64 {
        match self {
            Self::Live(r) => r.tokens_in,
            Self::Cached(c) => c.tokens_in,
        }
    }
    fn tokens_out(&self) -> u64 {
        match self {
            Self::Live(r) => r.tokens_out,
            Self::Cached(c) => c.tokens_out,
        }
    }
    fn latency_ms(&self) -> u64 {
        match self {
            Self::Live(r) => r.latency_ms,
            Self::Cached(c) => c.latency_ms,
        }
    }
    fn confidence(&self) -> f32 {
        match self {
            Self::Live(r) => r.confidence,
            Self::Cached(c) => c.confidence,
        }
    }
    fn provider(&self) -> &str {
        match self {
            Self::Live(r) => r.provider.as_str(),
            Self::Cached(c) => c.provider.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Composition inputs for a session run.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Mission root: session dir is `<root>/.hesmos/sessions/<session_id>/` (the trace
    /// crate's layout); `responses/` lives inside it.
    pub root: PathBuf,
    pub policy: PolicySet,
}

/// How a run ended — everything the CLI summary and exit mapping need.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    pub final_state: SessionState,
    /// The reason code for reason-driven terminals. `None` = clean completion or an
    /// external interrupt (the CLI maps a Suspended-with-no-reason to 130).
    pub reason: Option<ReasonCode>,
    /// min(gate scores) over the session (SS-09 rule 2 view; `None` = no gate ran).
    pub min_score: Option<f32>,
    /// Boundaries that retried and then passed (W3-2 CONCERNS count).
    pub concerns: u32,
    pub commits: u64,
    /// Hash of the last appended event (the pre-seal chain head).
    pub chain_head: Option<Sha256Hex>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    // `{0:?}` + manual From: CompileError deliberately has neither Display nor Error
    // impls (core formats it with Debug — HesmosError::compile does the same); the CLI
    // re-renders CE-* properly. `?` on compile results still works via the From impl.
    #[error("plan compile failed: {0:?}")]
    Compile(CompileError),
    #[error("session wal: {0}")]
    Wal(#[from] crate::wal::WalError),
    #[error("session io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<CompileError> for RunnerError {
    fn from(e: CompileError) -> Self {
        Self::Compile(e)
    }
}

/// Plan adjacency in deterministic BTree form — the runner's routing map.
struct Graph {
    task: String,
    stages: BTreeMap<NodeId, hesmos_core::StageSpec>,
    succs: BTreeMap<NodeId, BTreeSet<NodeId>>,
    preds: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

impl Graph {
    fn from_plan(plan: &Plan) -> Self {
        let mut stages = BTreeMap::new();
        for s in &plan.stages {
            stages.insert(s.id.clone(), s.clone());
        }
        let mut succs: BTreeMap<NodeId, BTreeSet<NodeId>> = stages
            .keys()
            .cloned()
            .map(|k| (k, BTreeSet::new()))
            .collect();
        let mut preds: BTreeMap<NodeId, BTreeSet<NodeId>> = stages
            .keys()
            .cloned()
            .map(|k| (k, BTreeSet::new()))
            .collect();
        for e in &plan.edges {
            succs
                .get_mut(&e.from)
                .expect("compiled edges reference compiled nodes")
                .insert(e.to.clone());
            preds
                .get_mut(&e.to)
                .expect("compiled edges reference compiled nodes")
                .insert(e.from.clone());
        }
        Self {
            task: plan.task.clone(),
            stages,
            succs,
            preds,
        }
    }
}

/// How a node boundary ended (the wave loop's control vocabulary).
enum NodeEnd {
    Committed,
    /// Budget family or external interrupt — Suspended (resume = last checkpoint).
    /// `None` reason = interrupt, not a budget event.
    Suspended(Option<ReasonCode>),
    /// GATE_REJECT — Failed (resume = fix + rerun).
    Failed(ReasonCode),
    /// Loop guard / provider family — Halted (resume = fork).
    Halted(ReasonCode),
}

/// Tail result of the route+commit phase: `Retry` re-enters the attempt loop (a
/// route-level bounded retry), `Done` ends the boundary.
enum RouteOutcome {
    Done(NodeEnd),
    Retry,
}

pub struct Runner {
    config: RunnerConfig,
    handle: SessionHandle,
    fork: Option<ForkSource>,
    graph: Graph,
    compiled: CompiledGraph,
    wal: SessionWal,
    executor: Box<dyn StageExecutor>,
    gates: Box<dyn GateConductor>,
    budget: Box<dyn BudgetConductor>,
    router: Box<dyn HandoffRouter>,
    cache: Box<dyn CacheConductor>,
    sink: Box<dyn EventSink>,
    /// Contract that routed INTO a node — stored with that node's checkpoint and reused
    /// as the boundary contract for its post-gates.
    routed_in: RefCell<BTreeMap<NodeId, HandoffContract>>,
    /// Output envelope per committed node (the input source for successors).
    outputs: RefCell<BTreeMap<NodeId, Envelope>>,
    /// Bounded-retry attempt counter per node (runner-local; budget =
    /// `policy.bounded_retry`, the SS-10 rule-3 single knob).
    attempts: RefCell<BTreeMap<NodeId, u8>>,
    /// Session aggregates for [`RunOutcome`].
    min_score: RefCell<Option<f32>>,
    concerns: RefCell<u32>,
    chain_head: RefCell<Option<Sha256Hex>>,
    /// Shared so the CLI's SIGINT handler can flip the same flag the run loop polls
    /// (`interrupt_handle`) — the process never dies mid-boundary; it suspends.
    interrupt: Arc<AtomicBool>,
}

impl Runner {
    /// S02–S04: compile, open the session store, emit session.open + plan.compiled.
    ///
    /// Compile runs FIRST and before any directory exists, so a CE-* failure leaves no
    /// session behind (exit 3 with 세션 없음). The session id is minted by the CALLER —
    /// the composition root owns id minting because it must open the EventLog and the
    /// metering ledger under `<root>/.hesmos/sessions/<id>/` BEFORE the runner runs
    /// (fresh sessions: `SessionId::generate()`; forks: [`derive_fork_session_id`]).
    /// Fork lineage is stamped on the handle from `fork` (TYPE-5).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        config: RunnerConfig,
        plan: &Plan,
        session_id: SessionId,
        seed: u64,
        budget: BudgetEnvelope,
        team_id: Option<TeamId>,
        fork: Option<ForkSource>,
        executor: Box<dyn StageExecutor>,
        gates: Box<dyn GateConductor>,
        budget_conductor: Box<dyn BudgetConductor>,
        router: Box<dyn HandoffRouter>,
        cache: Box<dyn CacheConductor>,
        sink: Box<dyn EventSink>,
    ) -> Result<Self, RunnerError> {
        let compiled = DeterministicEngine.compile(plan, seed)?;

        let session_dir = config
            .root
            .join(".hesmos")
            .join("sessions")
            .join(session_id.to_string());
        let wal = SessionWal::open(&session_dir.join("checkpoint.db"))?;

        // ERD §4's fourth artifact: the compiled-plan snapshot, the recompute source for
        // plan_hash. `trace replay` rebuilds the graph from THIS file (CLI-3 has no
        // <plan> argument — the origin's plan text must live with the session).
        std::fs::write(
            session_dir.join("plan_compiled.yaml"),
            serde_yaml_ng::to_string(plan)
                .map_err(|e| RunnerError::Io(std::io::Error::other(e.to_string())))?,
        )?;

        let mut runner = Self {
            handle: SessionHandle {
                session_id,
                run_id: RunId::generate(),
                seed,
                plan_hash: compiled.plan_hash.clone(),
                budget,
                state: SessionState::Init,
                team_id,
                fork_of: fork.as_ref().map(|f| (f.origin, f.at)),
                chain_head: None,
            },
            fork,
            graph: Graph::from_plan(plan),
            compiled,
            wal,
            config,
            executor,
            gates,
            budget: budget_conductor,
            router,
            cache,
            sink,
            routed_in: RefCell::new(BTreeMap::new()),
            outputs: RefCell::new(BTreeMap::new()),
            attempts: RefCell::new(BTreeMap::new()),
            min_score: RefCell::new(None),
            concerns: RefCell::new(0),
            chain_head: RefCell::new(None),
            interrupt: Arc::new(AtomicBool::new(false)),
        };
        // The sessions row exists from the first moment (update_state asserts it); the
        // log opens with the session facts, then the plan identity (S02/S04; the compile
        // itself — S03 — already happened above, before any state existed).
        runner.wal.insert_session(&runner.handle)?;
        runner.emit_session_open();
        runner.emit(
            EventKind::PlanCompiled,
            None,
            EventAttrs::new()
                .set("plan_hash", runner.compiled.plan_hash.as_str())
                .set("node_count", runner.compiled.node_count as u64),
        );
        runner
            .wal
            .update_state(&runner.handle.session_id, SessionState::Running, None)?;
        runner
            .handle
            .transition(SessionState::Running)
            .expect("Init -> Running is ST-1 legal");
        Ok(runner)
    }

    /// The session's immutable reproduction basis — CLI summaries and the seal read it.
    pub fn handle(&self) -> &SessionHandle {
        &self.handle
    }

    /// Cooperative interrupt: checked between steps; the next check suspends the
    /// session (SIGINT → exit 130 at the CLI). No signal handling lives in the runner.
    pub fn interrupt(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
    }

    /// The flag behind [`Runner::interrupt`] — the CLI hands this to its SIGINT
    /// handler so an external signal reaches the same cooperative check.
    pub fn interrupt_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.interrupt)
    }

    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }

    /// S05–S16: the wave loop. session.close + the state write happen here; the SEAL
    /// is the composition root's (trace crate), after this returns.
    pub fn run(mut self) -> Result<RunOutcome, RunnerError> {
        let mut end: Option<(SessionState, Option<ReasonCode>)> = None;

        'waves: while let Some(wave) = DeterministicEngine.next_wave(&self.compiled) {
            if self.interrupted() {
                end = Some((SessionState::Suspended, None));
                break 'waves;
            }
            for node in &wave.nodes {
                if self.interrupted() {
                    end = Some((SessionState::Suspended, None));
                    break 'waves;
                }
                match self.run_node(node, wave.index)? {
                    NodeEnd::Committed => {}
                    NodeEnd::Suspended(reason) => {
                        end = Some((SessionState::Suspended, reason));
                        break 'waves;
                    }
                    NodeEnd::Failed(reason) => {
                        end = Some((SessionState::Failed, Some(reason)));
                        break 'waves;
                    }
                    NodeEnd::Halted(reason) => {
                        end = Some((SessionState::Halted, Some(reason)));
                        break 'waves;
                    }
                }
            }
        }

        let (final_state, reason) = end.unwrap_or((SessionState::Completed, None));
        self.handle.transition(final_state).unwrap_or_else(|e| {
            panic!("runner produced an ST-1-illegal terminal transition: {e:?}")
        });
        self.wal.update_state(
            &self.handle.session_id,
            final_state,
            self.chain_head.borrow().as_ref(),
        )?;
        self.emit(
            EventKind::SessionClose,
            None,
            EventAttrs::new()
                .set("session_id", self.handle.session_id.to_string())
                .set("final_state", format!("{final_state:?}").to_uppercase()),
        );

        Ok(RunOutcome {
            final_state,
            reason,
            min_score: *self.min_score.borrow(),
            concerns: *self.concerns.borrow(),
            commits: self.compiled.commits().count() as u64,
            chain_head: self.chain_head.borrow().clone(),
        })
    }

    /// One node boundary — the full pre / execute / post / route / commit sequence.
    fn run_node(&mut self, node: &NodeId, wave: WaveIndex) -> Result<NodeEnd, RunnerError> {
        let stage = self
            .graph
            .stages
            .get(node)
            .expect("wave nodes come from the compiled plan")
            .clone();

        // node.start fires ONCE per node — retries only re-enter the attempt loop.
        self.emit(
            EventKind::NodeStart,
            Some(node.clone()),
            EventAttrs::new()
                .set("node_id", node.as_str())
                .set("agent_role", stage.profile.role.as_str())
                .set("wave", wave.0 as u64),
        );

        let input_envelope = self.build_input_envelope(node, &stage);
        let routed_contract = self.routed_in.borrow().get(node).cloned();
        // The boundary contract feeds done_criteria/rubric; roots synthesize one from
        // the stage's own declaration (the plan is their basis — nobody routed into
        // them).
        let boundary_contract = routed_contract
            .clone()
            .unwrap_or_else(|| synthetic_boundary_contract(node, &stage));
        // SS-19 grantor: whoever EMITTED the boundary contract holds the authority the
        // contract's cap may not exceed (roots self-grant via the synthetic contract).
        // Cloned out of the graph so the borrow never fights route_and_commit's &mut.
        let grantor: Option<AgentProfile> = self
            .graph
            .stages
            .get(&boundary_contract.from_node)
            .map(|s| s.profile.clone());

        // —— S05/S06 pre-gates: G0 · permission · budget · schema ∪ stage.pre. ——
        let mut pre_refs: BTreeSet<String> = ["G0", "permission", "budget", "schema"]
            .into_iter()
            .map(String::from)
            .collect();
        pre_refs.extend(stage.gates.pre.iter().cloned());
        for gate_ref in &pre_refs {
            let verdict = self.gates.run_gate(GateRun {
                gate_ref,
                session: &self.handle,
                sink: self.sink.as_ref(),
                node,
                phase: if gate_ref == "G0" {
                    RunnerPhase::G0
                } else {
                    RunnerPhase::Pre
                },
                envelope_in: Some(&input_envelope),
                envelope_out: None,
                contract: Some(&boundary_contract),
                policy: &self.config.policy,
                budget_state: self.budget.snapshot(&stage.profile.role),
                attempt: self.attempt_of(node),
                session_task: &self.graph.task,
                spawn_token_floor: stage.profile.spawn_token_floor,
                grantor_profile: grantor.as_ref(),
                extra: &[],
            });
            self.record_score(&verdict);
            match verdict {
                GateVerdict::Pass { .. } => {}
                // The budget gate's rejection IS the suspend signal (SS-15 rule 4
                // pre-block): the session suspends at the last commit point.
                GateVerdict::Reject {
                    reason_code: ReasonCode::BUDGET_EXCEEDED,
                    ..
                } => {
                    let spent = self.budget.snapshot(&stage.profile.role).session_spent;
                    self.emit_budget_event("suspend", spent, Some(0));
                    self.stop_node(node, &stage, wave, "escalate");
                    return Ok(NodeEnd::Suspended(Some(ReasonCode::BUDGET_EXCEEDED)));
                }
                GateVerdict::Reject { .. } => {
                    self.stop_node(node, &stage, wave, "reject");
                    return Ok(NodeEnd::Failed(ReasonCode::GATE_REJECT));
                }
                // Baseline pre-gates are total functions over the request — nothing has
                // been executed yet, so there is nothing a Retry could re-run. Reaching
                // this arm is a composition bug and crashes instead of looping a no-op.
                GateVerdict::Retry { .. } => {
                    panic!("pre-gate {gate_ref} returned Retry — baseline pre-gates are total")
                }
            }
        }

        // —— S07–S14: the attempt loop. Bounded retry re-enters HERE only. ——
        let mut retried = false;
        let end: NodeEnd = loop {
            if self.interrupted() {
                self.stop_node(node, &stage, wave, "escalate");
                break NodeEnd::Suspended(None);
            }
            let attempt = self.attempt_of(node);

            // —— S07/S08 execute (or load the fork's cached execution). ——
            let output = match self.cached_execution(node)? {
                Some(cached) => NodeOutput::Cached(cached),
                None => match self.executor.execute(&StageRequest {
                    node: node.clone(),
                    wave,
                    attempt,
                    role: stage.profile.role.clone(),
                    model: stage.profile.model.clone(),
                    task: self.graph.task.clone(),
                    node_input: input_envelope.payload.json.clone(),
                }) {
                    Ok(report) => NodeOutput::Live(report),
                    Err(failure) => {
                        // Provider-failure family: bounded retry on the SAME budget
                        // (SS-10 rule 3); halt when it is spent (exit 12 shape).
                        if attempt < self.config.policy.bounded_retry {
                            self.bump_attempt(node);
                            retried = true;
                            continue;
                        }
                        self.stop_node(node, &stage, wave, "escalate");
                        break NodeEnd::Halted(failure.reason_code);
                    }
                },
            };

            // —— SS-18 cache sentinel: the turn's prompt hash, verified BEFORE the
            // turn is recorded. A violation halts IMMEDIATELY — the special
            // no-retry rule (exceptions §4): the session never enters the bounded
            // retry loop on this failure, and the turn leaves NO llm.call event /
            // meter row, so the ledger==events invariant (US-20 AC3) survives.
            if let NodeOutput::Live(report) = &output
                && let CacheVerdict::Violated = self.cache.verify_turn(report.prompt_hash.as_ref())
            {
                // gate_id "cache" is the display-layer key: the guard's
                // CACHE_GATE_ID and the trace renderer's CACHE_INVARIANT phrase
                // both derive from this spelling (ui-spec §8.2) — keep them
                // equal. reason_code stays in the 6-kind registry by contract.
                self.record_score_value(0.0);
                self.emit(
                    EventKind::GateFail,
                    Some(node.clone()),
                    EventAttrs::new()
                        .set("gate_id", "cache")
                        .set(
                            "reason_code",
                            hesmos_core::code_to_static(ReasonCode::GATE_REJECT),
                        )
                        .set("score", 0.0),
                );
                self.stop_node(node, &stage, wave, "reject");
                return Ok(NodeEnd::Failed(ReasonCode::GATE_REJECT));
            }

            // —— S08 llm.call + S15 metering. A WARN fires immediately; a SUSPEND
            // waits for the commit boundary (see module doc). ——
            self.emit(
                EventKind::LlmCall,
                Some(node.clone()),
                EventAttrs::new()
                    .set("provider", output.provider())
                    .set("model", stage.profile.model.as_str())
                    .set("tokens_in", output.tokens_in())
                    .set("tokens_out", output.tokens_out())
                    .set("latency_ms", output.latency_ms()),
            );
            if let SpendVerdict::Warn { spent, remaining } = self.budget.meter(
                &stage.profile.role,
                MeterKind::Llm,
                output.tokens_in(),
                output.tokens_out(),
            ) {
                self.emit_budget_event("warn", spent, remaining);
            }

            let confidence = Confidence::new(output.confidence().clamp(0.0, 1.0))
                .expect("executor confidence clamped into range");
            let out = match &output {
                NodeOutput::Cached(c) => c.envelope.clone(),
                NodeOutput::Live(report) => {
                    self.build_output_envelope(node, report, &input_envelope)
                }
            };

            // —— S11 post-gates: schema · done_criteria · rubric ∪ stage.post. ——
            let mut post_refs: BTreeSet<String> = ["schema", "done_criteria", "rubric"]
                .into_iter()
                .map(String::from)
                .collect();
            post_refs.extend(stage.gates.post.iter().cloned());
            let mut first_non_pass: Option<(String, GateVerdict)> = None;
            for gate_ref in &post_refs {
                let verdict = self.gates.run_gate(GateRun {
                    gate_ref,
                    session: &self.handle,
                    sink: self.sink.as_ref(),
                    node,
                    phase: RunnerPhase::Post,
                    envelope_in: Some(&input_envelope),
                    envelope_out: Some(&out),
                    contract: Some(&boundary_contract),
                    policy: &self.config.policy,
                    budget_state: self.budget.snapshot(&stage.profile.role),
                    attempt,
                    session_task: &self.graph.task,
                    spawn_token_floor: stage.profile.spawn_token_floor,
                    grantor_profile: grantor.as_ref(),
                    extra: &[],
                });
                self.record_score(&verdict);
                if !matches!(verdict, GateVerdict::Pass { .. }) {
                    first_non_pass = Some((gate_ref.clone(), verdict));
                    break;
                }
            }

            match first_non_pass.as_ref() {
                None => {
                    // Every post-gate passed → route (S13) → commit (S14).
                    match self.route_and_commit(
                        node,
                        &stage,
                        wave,
                        out,
                        confidence,
                        output,
                        routed_contract.as_ref(),
                        retried,
                    )? {
                        RouteOutcome::Done(end) => break end,
                        RouteOutcome::Retry => {
                            // Route-level bounded retry: the sender re-executes under
                            // the ROUTER's per-sender budget; exhaustion arrives as a
                            // ReturnToSender decision, never an infinite loop.
                            self.bump_attempt(node);
                            retried = true;
                            continue;
                        }
                    }
                }
                Some((_, GateVerdict::Retry { .. })) => {
                    if attempt < self.config.policy.bounded_retry {
                        self.bump_attempt(node);
                        retried = true;
                        continue;
                    }
                    // Budget spent: the exhausting attempt IS the escalation record —
                    // its final gate.fail carries escalated_to/escalation_reason
                    // (retry.rs). The parent is whoever routed into this node; roots
                    // escalate to themselves (no parent exists to receive it).
                    let parent = routed_contract
                        .as_ref()
                        .map(|c| c.from_node.clone())
                        .unwrap_or_else(|| node.clone());
                    let extra = [
                        ("escalated_to", serde_json::json!(parent.to_string())),
                        (
                            "escalation_reason",
                            serde_json::json!("bounded_retry_exhausted"),
                        ),
                    ];
                    // Re-run the failing gate once with the escalation attrs — the
                    // verdict is deterministic, so this is the same failure, now
                    // recorded as the escalation event.
                    let failing_ref = first_non_pass.as_ref().expect("checked above").0.clone();
                    let escalated = self.gates.run_gate(GateRun {
                        gate_ref: failing_ref.as_str(),
                        session: &self.handle,
                        sink: self.sink.as_ref(),
                        node,
                        phase: RunnerPhase::Post,
                        envelope_in: Some(&input_envelope),
                        envelope_out: Some(&out),
                        contract: Some(&boundary_contract),
                        policy: &self.config.policy,
                        budget_state: self.budget.snapshot(&stage.profile.role),
                        attempt,
                        session_task: &self.graph.task,
                        spawn_token_floor: stage.profile.spawn_token_floor,
                        grantor_profile: grantor.as_ref(),
                        extra: &extra,
                    });
                    self.record_score(&escalated);
                    self.stop_node(node, &stage, wave, "reject");
                    break NodeEnd::Failed(ReasonCode::GATE_REJECT);
                }
                Some((
                    _,
                    GateVerdict::Reject {
                        reason_code: ReasonCode::BUDGET_EXCEEDED,
                        ..
                    },
                )) => {
                    // A budget-scope rejection post-gate suspends (SS-08: the budget
                    // family never maps to Failed).
                    self.stop_node(node, &stage, wave, "escalate");
                    break NodeEnd::Suspended(Some(ReasonCode::BUDGET_EXCEEDED));
                }
                Some((_, GateVerdict::Reject { .. })) => {
                    self.stop_node(node, &stage, wave, "reject");
                    break NodeEnd::Failed(ReasonCode::GATE_REJECT);
                }
                // Unreachable by construction: first_non_pass only stores non-pass
                // verdicts (the post-gate loop breaks on the first one).
                Some((_, GateVerdict::Pass { .. })) => {
                    unreachable!("first_non_pass never stores a Pass verdict")
                }
            }
        };
        Ok(end)
    }

    /// S13 route (one handoff per outgoing edge, BTree order) then S14 commit.
    // The eight parameters ARE the boundary state at commit time (identity, stage,
    // output, judgment, provenance, retry bookkeeping) — bundling them into a struct
    // would just move the same names one level down.
    #[allow(clippy::too_many_arguments)]
    fn route_and_commit(
        &mut self,
        node: &NodeId,
        stage: &hesmos_core::StageSpec,
        wave: WaveIndex,
        out: Envelope,
        confidence: Confidence,
        output: NodeOutput,
        routed_contract: Option<&HandoffContract>,
        retried: bool,
    ) -> Result<RouteOutcome, RunnerError> {
        let successors: Vec<NodeId> = self
            .graph
            .succs
            .get(node)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        // The router.v1 gate.fail score is the confidence the contract carried — read
        // it before the confidence value moves into the last contract.
        let contract_score = confidence.get();
        for to in successors {
            let contract = self.build_handoff_contract(node, &to, stage, &out, confidence);
            let hash = contract.contract_hash();
            self.emit(
                EventKind::HandoffRequest,
                Some(node.clone()),
                EventAttrs::new()
                    .set("contract_hash", hash.as_str())
                    .set("from", node.as_str()),
            );
            match self.router.route(contract.clone()) {
                RouteDecision::Accept { to, contract_hash } => {
                    self.emit(
                        EventKind::HandoffAccept,
                        Some(node.clone()),
                        EventAttrs::new()
                            .set("contract_hash", contract_hash.as_str())
                            .set("from", node.as_str())
                            .set("to", to.as_str()),
                    );
                    // The destination's boundary contract is THIS handoff — its
                    // post-gates judge against it and its checkpoint stores it.
                    self.routed_in.borrow_mut().insert(to, contract);
                }
                RouteDecision::RetryBounded { .. } => {
                    // Contract confidence below the floor: re-enter the attempt loop.
                    // The router keeps the per-sender budget internally and answers
                    // ReturnToSender on exhaustion, so this cannot loop forever.
                    return Ok(RouteOutcome::Retry);
                }
                RouteDecision::ReturnToSender { reason_code, .. } => {
                    // Rejected contract (or exhausted route budget): the sender's
                    // output never commits — the boundary ends FAILED, and the
                    // judgment leaves the router.v1 gate.fail event.
                    let attrs = EventAttrs::new()
                        .set("gate_id", "router.v1")
                        .set("reason_code", hesmos_core::code_to_static(reason_code))
                        .set("score", contract_score);
                    self.emit(EventKind::GateFail, Some(node.clone()), attrs);
                    self.record_score_value(contract_score);
                    self.stop_node(node, stage, wave, "reject");
                    return Ok(RouteOutcome::Done(NodeEnd::Failed(ReasonCode::GATE_REJECT)));
                }
                RouteDecision::Halt { reason_code } => {
                    let attrs = EventAttrs::new()
                        .set("gate_id", "router.v1")
                        .set("reason_code", hesmos_core::code_to_static(reason_code))
                        .set("score", 0.0);
                    self.emit(EventKind::GateFail, Some(node.clone()), attrs);
                    self.record_score_value(0.0);
                    self.stop_node(node, stage, wave, "escalate");
                    return Ok(RouteOutcome::Done(NodeEnd::Halted(reason_code)));
                }
            }
        }

        // —— S14 commit: engine receipt → WAL checkpoint row → response cache. ——
        let receipt = DeterministicEngine.commit(
            &mut self.compiled,
            StageOutput {
                node: node.clone(),
                envelope: out.clone(),
                confidence,
            },
        );
        let cached = CachedExecution {
            envelope: out.clone(),
            confidence: output.confidence(),
            tokens_in: output.tokens_in(),
            tokens_out: output.tokens_out(),
            latency_ms: output.latency_ms(),
            provider: output.provider().to_string(),
        };
        self.persist_commit(&receipt, node, &out, cached, routed_contract)?;
        // The committed envelope is the successor's input source (the field's own
        // contract — "output envelope per committed node"). WP-P2a caught this write
        // missing since P1e: the map stayed empty forever, so every node's merged
        // input silently lost its predecessors' payloads (and their taint). The
        // taint-chain test fails loudly without this line.
        self.outputs.borrow_mut().insert(node.clone(), out);

        // S15 post-commit budget check — at the commit boundary so the suspend always
        // has a checkpoint to resume from.
        if let SpendVerdict::Exceeded = self.budget.meter(&stage.profile.role, MeterKind::Llm, 0, 0)
        {
            let spent = self.budget.snapshot(&stage.profile.role).session_spent;
            self.emit_budget_event("suspend", spent, Some(0));
            self.stop_node(node, stage, wave, "escalate");
            return Ok(RouteOutcome::Done(NodeEnd::Suspended(Some(
                ReasonCode::BUDGET_EXCEEDED,
            ))));
        }

        if retried {
            *self.concerns.borrow_mut() += 1;
        }
        self.stop_node(node, stage, wave, "done");
        Ok(RouteOutcome::Done(NodeEnd::Committed))
    }

    fn attempt_of(&self, node: &NodeId) -> u8 {
        *self.attempts.borrow().get(node).unwrap_or(&0)
    }

    fn bump_attempt(&self, node: &NodeId) {
        let attempt = self.attempt_of(node);
        self.attempts.borrow_mut().insert(node.clone(), attempt + 1);
    }

    fn stop_node(
        &self,
        node: &NodeId,
        stage: &hesmos_core::StageSpec,
        wave: WaveIndex,
        stop_kind: &'static str,
    ) {
        self.emit(
            EventKind::NodeStop,
            Some(node.clone()),
            EventAttrs::new()
                .set("node_id", node.as_str())
                .set("agent_role", stage.profile.role.as_str())
                .set("wave", wave.0 as u64)
                .set("stop_kind", stop_kind),
        );
    }

    fn emit_budget_event(&self, level: &'static str, spent: u64, remaining: Option<u64>) {
        self.emit(
            EventKind::BudgetEvent,
            None,
            EventAttrs::new()
                .set("level", level)
                .set("spent", spent)
                .set("remaining", remaining.unwrap_or(0)),
        );
    }

    fn emit(&self, kind: EventKind, node: Option<NodeId>, attrs: EventAttrs) {
        let pending =
            PendingEvent::new(kind, node, attrs).expect("runner events carry complete attrs");
        let appended = self.sink.emit(pending);
        *self.chain_head.borrow_mut() = Some(appended.hash);
    }

    fn record_score(&self, verdict: &GateVerdict) {
        let score = match verdict {
            GateVerdict::Pass { score }
            | GateVerdict::Retry { score, .. }
            | GateVerdict::Reject { score, .. } => *score,
        };
        self.record_score_value(score);
    }

    fn record_score_value(&self, score: f32) {
        let mut min = self.min_score.borrow_mut();
        *min = Some(min.map_or(score, |m: f32| m.min(score)));
    }

    /// The node's input envelope: task + merged predecessor payloads. Roots receive the
    /// task itself as their transfer (G0/schema judge it like any input). Ids derive
    /// from the wave ordinal — no entropy at the boundary. Taint is the MERGE of the
    /// predecessor outputs (S5 over a multi-source assembly — `Taint::merge`): any
    /// Tainted input marks the whole transfer, so contamination cannot be laundered by
    /// merging.
    fn build_input_envelope(&self, node: &NodeId, stage: &hesmos_core::StageSpec) -> Envelope {
        let ordinal = self.ordinal(node);
        let mut inputs = serde_json::Map::new();
        let mut pred_taints: Vec<hesmos_core::Taint> = Vec::new();
        if let Some(preds) = self.graph.preds.get(node) {
            let outputs = self.outputs.borrow();
            for pred in preds {
                if let Some(env) = outputs.get(pred) {
                    pred_taints.push(env.taint.clone());
                    inputs.insert(
                        pred.to_string(),
                        serde_json::json!({
                            "schema_id": env.payload.schema_id.as_str(),
                            "json": env.payload.json,
                        }),
                    );
                }
            }
        }
        let node_input = serde_json::json!({
            "task": self.graph.task.clone(),
            "inputs": inputs,
        });
        let from = self
            .graph
            .preds
            .get(node)
            .and_then(|p| p.iter().next().cloned())
            .unwrap_or_else(|| node.clone());
        Envelope {
            id: EnvelopeId::from_u128(ordinal),
            from,
            to: node.clone(),
            kind: EnvelopeKind::Task,
            payload: Payload {
                schema_id: stage.input_schema.clone(),
                json: node_input,
            },
            correlation_id: CorrelationId::from_u128(ordinal),
            taint: hesmos_core::Taint::merge(&pred_taints),
        }
    }

    /// The output envelope: DERIVED from the input envelope through
    /// [`Envelope::derive`] — the sanctioned derivation path, so input taint flows to
    /// the output unconditionally (S5). A declared-external output (`ExecutionReport::
    /// external_origin`, SS-20 rule 1) is then marked via the only sanctioned
    /// Clean→Tainted path; the assembly can therefore never produce an unmarked
    /// external envelope.
    fn build_output_envelope(
        &self,
        node: &NodeId,
        report: &ExecutionReport,
        input: &Envelope,
    ) -> Envelope {
        let ordinal = self.ordinal(node);
        // Output envelope `to`: the sole successor when there is exactly one, else the
        // node itself (fan-out destinations resolve per-edge at routing).
        let succs = self.graph.succs.get(node);
        let to = match succs.map(|s| s.len()) {
            Some(1) => succs
                .and_then(|s| s.iter().next())
                .cloned()
                .unwrap_or_else(|| node.clone()),
            _ => node.clone(),
        };
        let mut out = input.derive(
            EnvelopeId::from_u128(ordinal * 2),
            to,
            EnvelopeKind::Result,
            Payload {
                schema_id: SchemaId::new("out.v1"),
                json: report.payload.clone(),
            },
            CorrelationId::from_u128(ordinal * 2),
        );
        if let Some(origin) = &report.external_origin {
            // SS-20 rule 1: a declared-external CLEAN output is the rejected state —
            // resolve it by marking (the only sanctioned Clean→Tainted path). An
            // already-Tainted output keeps its first source (first-mark-wins).
            if hesmos_core::external_import_verdict(true, &out.taint, origin).is_err() {
                out.mark_tainted(hesmos_core::TaintSource {
                    origin: origin.clone(),
                });
            }
        }
        out
    }

    /// The routed handoff for edge from→to: goal chain from the session task, the
    /// DESTINATION's done_criteria (what `to` must show), and the destination's
    /// permission surface (SS-19: caps travel with the work).
    fn build_handoff_contract(
        &self,
        from: &NodeId,
        to: &NodeId,
        stage: &hesmos_core::StageSpec,
        out: &Envelope,
        confidence: Confidence,
    ) -> HandoffContract {
        let target = self.graph.stages.get(to);
        HandoffContract {
            goal_original: self.graph.task.clone(),
            goal_current: format!("handoff to `{to}`"),
            // The runner AUTHORS the contract in the echo composition, so it must be
            // matrix-clean by construction (guard rows 3/7 reject empty invariants /
            // absent assumptions — a contract that fails validation on every handoff
            // would make no plan completable). The invariant stated is one the runner
            // genuinely enforces (guard row 1); empty assumptions is the truthful
            // "nothing provisional asserted". Real executors (W6) replace these
            // synthesized fields with executor-reported ones; artifacts stays None and
            // keeps its designed soft consequence (post_gate_fail, accepted at route).
            invariants: vec![HANDOFF_INVARIANT.into()],
            done_criteria: target.map(|s| s.done_criteria.clone()),
            artifacts: None,
            failed_approaches: None,
            assumptions: Some(vec![]),
            confidence: Some(confidence),
            from_node: from.clone(),
            to_node: Some(to.clone()),
            permission_cap: hesmos_core::PermissionCap {
                tools: target.map(|s| s.profile.tools.clone()).unwrap_or_default(),
                network_allow: target
                    .map(|s| s.profile.network.allow.clone())
                    .unwrap_or_default(),
            },
            snapshot: hesmos_core::SnapshotSlice {
                goal_original: self.graph.task.clone(),
                prev_contract_summary: String::new(),
                node_input: Payload {
                    schema_id: stage.input_schema.clone(),
                    json: out.payload.json.clone(),
                },
                shared_knowledge: vec![],
            },
        }
    }

    /// Wave-ordinal of a node (the seed-fixed compile order) — the entropy-free source
    /// for envelope/correlation ids.
    fn ordinal(&self, node: &NodeId) -> u128 {
        let mut ord = 0u128;
        for wave in self.compiled.waves() {
            for n in &wave.nodes {
                ord += 1;
                if n == node {
                    return ord;
                }
            }
        }
        unreachable!("ordinal of a node outside the compiled graph")
    }

    /// The fork's cached execution for the next commit, if this run is a fork whose
    /// replay window still covers it.
    ///
    /// A read miss (no cache file for this seq) falls back to LIVE execution — the
    /// contract's 구조 단위 reuse (A5): with a deterministic executor the bytes come
    /// out identical anyway, and demanding cache rows for every seq would make
    /// partial-cache origins unforkable. Only non-NotFound I/O errors propagate.
    fn cached_execution(&self, node: &NodeId) -> Result<Option<CachedExecution>, RunnerError> {
        let Some(fork) = &self.fork else {
            return Ok(None);
        };
        let next_seq = self.compiled.commits().count() as u64 + 1;
        if next_seq > fork.at.0 {
            return Ok(None);
        }
        let path = fork.origin_responses.join(format!("{next_seq}.json"));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let cached: CachedExecution = serde_json::from_slice(&bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("cache @{next_seq}: {e}"),
            )
        })?;
        debug_assert_eq!(
            cached.envelope.from.as_str(),
            node.as_str(),
            "cache row {next_seq} belongs to a different node"
        );
        Ok(Some(cached))
    }

    /// Engine receipt → WAL checkpoint row → response cache file. The event append
    /// happened EARLIER in the boundary (backend.md decision #11): a WAL failure leaves
    /// extra events but no commit, and replay trusts only checkpoint rows — safe in
    /// both directions. `ts` is an aggregation aid and never a hash input (D4).
    fn persist_commit(
        &self,
        receipt: &CommitReceipt,
        node: &NodeId,
        envelope: &Envelope,
        cached: CachedExecution,
        routed_contract: Option<&HandoffContract>,
    ) -> Result<(), RunnerError> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_millis() as i64;
        self.wal.commit_point(
            &self.handle.session_id,
            receipt.commit_seq,
            node,
            envelope,
            routed_contract,
            ts,
        )?;
        let dir = self
            .config
            .root
            .join(".hesmos")
            .join("sessions")
            .join(self.handle.session_id.to_string())
            .join("responses");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join(format!("{}.json", receipt.commit_seq.0)),
            serde_json::to_vec(&cached).expect("cache row serializes"),
        )?;
        Ok(())
    }

    fn emit_session_open(&self) {
        let budget_text = match self.handle.budget.session_max_tokens {
            Some(n) => format!("tokens={n}"),
            None => "tokens=unbounded".to_string(),
        };
        self.emit(
            EventKind::SessionOpen,
            None,
            EventAttrs::new()
                .set("session_id", self.handle.session_id.to_string())
                .set("seed", self.handle.seed)
                .set("budget", budget_text),
        );
    }
}

/// Root boundary contract: the stage's own done_criteria is the judging basis. Only the
/// criteria are read by the post-gates; no confidence is claimed (nothing routed here,
/// so no confidence exists to carry).
fn synthetic_boundary_contract(node: &NodeId, stage: &hesmos_core::StageSpec) -> HandoffContract {
    HandoffContract {
        goal_original: String::new(),
        goal_current: String::new(),
        invariants: vec![],
        done_criteria: Some(stage.done_criteria.clone()),
        artifacts: None,
        failed_approaches: None,
        assumptions: None,
        confidence: None,
        from_node: node.clone(),
        to_node: Some(node.clone()),
        permission_cap: hesmos_core::PermissionCap {
            tools: BTreeSet::<ToolName>::new(),
            network_allow: BTreeSet::new(),
        },
        snapshot: hesmos_core::SnapshotSlice {
            goal_original: String::new(),
            prev_contract_summary: String::new(),
            node_input: Payload {
                schema_id: stage.input_schema.clone(),
                json: serde_json::json!({}),
            },
            shared_knowledge: vec![],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // -- observation doubles -----------------------------------------------

    /// Records every emitted event. The orchestrator cannot depend on the trace crate
    /// (D-2), so runner tests observe the sink seam directly; the REAL chain (hashes,
    /// seal) is proven in `tests/integration_rust`.
    struct RecordingSink {
        events: Mutex<Vec<(EventKind, Option<NodeId>, EventAttrs)>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }
        fn kinds(&self) -> Vec<EventKind> {
            self.events
                .lock()
                .expect("lock")
                .iter()
                .map(|(k, _, _)| *k)
                .collect()
        }
        fn attrs_of(&self, kind: EventKind) -> Vec<EventAttrs> {
            self.events
                .lock()
                .expect("lock")
                .iter()
                .filter(|(k, _, _)| *k == kind)
                .map(|(_, _, a)| a.clone())
                .collect()
        }
    }
    impl EventSink for RecordingSink {
        fn emit(&self, e: PendingEvent) -> hesmos_core::TraceEvent {
            self.events
                .lock()
                .expect("lock")
                .push((e.kind, e.node.clone(), e.attrs.clone()));
            hesmos_core::TraceEvent {
                seq: 0,
                kind: e.kind,
                node: e.node,
                attrs: e.attrs,
                prev_hash: Sha256Hex::parse("0".repeat(64)).expect("hex"),
                hash: Sha256Hex::parse("0".repeat(64)).expect("hex"),
                ts: 0,
            }
        }
    }

    /// Shareable handle: `Runner::open` takes `Box<dyn EventSink>` by value, so tests
    /// hold an `Arc` to the recorder and hand the runner a delegating clone.
    #[derive(Clone)]
    struct SinkHandle(Arc<RecordingSink>);
    impl EventSink for SinkHandle {
        fn emit(&self, e: PendingEvent) -> hesmos_core::TraceEvent {
            self.0.emit(e)
        }
    }

    /// Scripted gate conductor: answers each run with the next preset verdict
    /// (Pass when the script is empty) and records what it was asked to run.
    struct ScriptedGates {
        answers: Mutex<Vec<GateVerdict>>,
        seen: Mutex<Vec<(String, RunnerPhase)>>,
    }
    impl ScriptedGates {
        fn all_pass() -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(Vec::new()),
                seen: Mutex::new(Vec::new()),
            })
        }
        /// `p` pass, `r` retry, `j` reject — a compact script per scheduled gate run.
        fn scripted(script: &str) -> Arc<Self> {
            let answers = script
                .chars()
                .filter(|c| !c.is_whitespace())
                .map(|c| match c {
                    'r' => GateVerdict::Retry {
                        reason_code: ReasonCode::GATE_REJECT,
                        score: 0.4,
                        attempts_left: 1,
                    },
                    'j' => GateVerdict::Reject {
                        reason_code: ReasonCode::GATE_REJECT,
                        score: 0.0,
                    },
                    _ => GateVerdict::Pass { score: 1.0 },
                })
                .collect();
            Arc::new(Self {
                answers: Mutex::new(answers),
                seen: Mutex::new(Vec::new()),
            })
        }
    }
    impl GateConductor for ScriptedGates {
        fn run_gate(&self, req: GateRun<'_>) -> GateVerdict {
            self.seen
                .lock()
                .expect("lock")
                .push((req.gate_ref.to_string(), req.phase));
            let mut answers = self.answers.lock().expect("lock");
            if answers.is_empty() {
                GateVerdict::Pass { score: 1.0 }
            } else {
                answers.remove(0)
            }
        }
    }

    #[derive(Clone)]
    struct GatesHandle(Arc<ScriptedGates>);
    impl GateConductor for GatesHandle {
        fn run_gate(&self, req: GateRun<'_>) -> GateVerdict {
            self.0.run_gate(req)
        }
    }

    /// In-memory budget conductor: every metered call costs its tokens plus a fixed
    /// surcharge; the suspend line sits at 1000.
    struct FakeBudget(u64, Mutex<u64>);
    impl FakeBudget {
        fn new(surcharge: u64) -> Self {
            Self(surcharge, Mutex::new(0))
        }
        fn spent(&self) -> u64 {
            *self.1.lock().expect("lock")
        }
    }
    impl BudgetConductor for FakeBudget {
        fn snapshot(&self, _role: &AgentRole) -> BudgetState {
            BudgetState {
                session_spent: self.spent(),
                session_warn_limit: None,
                session_suspend_limit: Some(1000),
                team_spent: 0,
                team_suspend_limit: None,
                agent_spent: 0,
                agent_suspend_limit: None,
            }
        }
        fn meter(&self, _role: &AgentRole, _kind: MeterKind, tin: u64, tout: u64) -> SpendVerdict {
            let mut spent = self.1.lock().expect("lock");
            *spent += tin + tout + self.0;
            if *spent >= 1000 {
                SpendVerdict::Exceeded
            } else {
                SpendVerdict::Clear
            }
        }
    }

    /// Validator double mirroring the real guard's below-floor branch (hesmos-guard's
    /// contract_check): confidence under the floor → BoundedRetry, otherwise Accept.
    /// With the default floor 0.0 it accepts everything, so happy-path tests are
    /// unaffected; the route-rejection test raises the floor to exercise the branch.
    struct FloorValidator;
    impl crate::router::ContractValidator for FloorValidator {
        fn validate(
            &self,
            contract: &HandoffContract,
            _task: &str,
            floor: f32,
        ) -> hesmos_core::ContractJudgment {
            match contract.confidence {
                Some(c) if c.get() < floor => hesmos_core::ContractJudgment::BoundedRetry,
                _ => hesmos_core::ContractJudgment::Accept,
            }
        }
    }

    // -- fixtures ----------------------------------------------------------

    const PLAN_YAML: &str = r#"
name: runner-e2e
task: write the report
pattern: graph
stages:
  - id: research
    profile: {role: researcher, model: glm-5.3-flash}
    input_schema: research.v1
    done_criteria: {items: [notes]}
  - id: draft
    profile: {role: writer, model: glm-5.3-flash}
    input_schema: draft.v1
    done_criteria: {items: [draft]}
flow: "research -> draft"
"#;

    const CHAIN3_YAML: &str = r#"
name: chain3
task: relay
pattern: graph
stages:
  - id: a
    profile: {role: researcher, model: glm-5.3-flash}
    input_schema: a.v1
    done_criteria: {items: [x]}
  - id: b
    profile: {role: writer, model: glm-5.3-flash}
    input_schema: b.v1
    done_criteria: {items: [y]}
  - id: c
    profile: {role: reviewer, model: glm-5.3-flash}
    input_schema: c.v1
    done_criteria: {items: [z]}
flow: "a -> b; b -> c"
"#;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hesmos-runner-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn accept_router(task: &str, policy: PolicySet) -> Box<dyn HandoffRouter> {
        Box::new(crate::router::LoopGuardRouter::new(
            task,
            FloorValidator,
            policy,
        ))
    }

    /// Cache sentinel stub — `Stable` forever by default, or always `Violated` for
    /// the SS-18 halt tests (WP-P2a).
    struct FixedCache(bool);
    impl CacheConductor for FixedCache {
        fn verify_turn(&self, _reported: Option<&Sha256Hex>) -> CacheVerdict {
            if self.0 {
                CacheVerdict::Violated
            } else {
                CacheVerdict::Stable
            }
        }
    }

    struct Built {
        runner: Runner,
        sink: SinkHandle,
        gates: Option<Arc<ScriptedGates>>,
        root: PathBuf,
        session_id: SessionId,
    }

    fn build(
        name: &str,
        yaml: &str,
        mode: EchoMode,
        policy: PolicySet,
        gates: Option<Arc<ScriptedGates>>,
        budget: Box<dyn BudgetConductor>,
    ) -> Built {
        build_with_executor(
            name,
            yaml,
            Box::new(EchoExecutor::new(mode)),
            policy,
            gates,
            budget,
        )
    }

    /// The same rig with a CUSTOM executor — the seam a real adapter will occupy, so
    /// executor-reported facts (provider identity, prompt hash) are provable end to
    /// end instead of only through the echo double's fixed values.
    fn build_with_executor(
        name: &str,
        yaml: &str,
        executor: Box<dyn StageExecutor>,
        policy: PolicySet,
        gates: Option<Arc<ScriptedGates>>,
        budget: Box<dyn BudgetConductor>,
    ) -> Built {
        let root = temp_root(name);
        let plan = crate::compile::parse_plan(yaml).expect("plan parses");
        let sink = SinkHandle(Arc::new(RecordingSink::new()));
        let gates_handle = gates.clone().map(GatesHandle);
        // One PolicySet everywhere (composition reality: the CLI builds a single set):
        // the router's loop guards and floors read ITS policy, the runner's reads its
        // config — diverging them would make these tests lie.
        let runner = Runner::open(
            RunnerConfig {
                root: root.clone(),
                policy: policy.clone(),
            },
            &plan,
            SessionId::generate(),
            42,
            BudgetEnvelope::default(),
            None,
            None,
            executor,
            Box::new(gates_handle.expect("gates required")),
            budget,
            accept_router("write the report", policy),
            Box::new(FixedCache(false)),
            Box::new(sink.clone()),
        )
        .expect("open");
        let session_id = runner.handle().session_id;
        Built {
            runner,
            sink,
            gates,
            root,
            session_id,
        }
    }

    // -- tests ---------------------------------------------------------------

    /// The full happy path: boundary events in order, one start/stop per node, the
    /// routed handoff between the two nodes, dense commits, COMPLETED close.
    #[test]
    fn happy_path_walks_the_full_boundary_sequence() {
        let built = build(
            "happy",
            PLAN_YAML,
            EchoMode::Echo,
            PolicySet::default(),
            Some(ScriptedGates::all_pass()),
            Box::new(FakeBudget::new(0)),
        );
        let outcome = built.runner.run().expect("run");

        assert_eq!(outcome.final_state, SessionState::Completed);
        assert_eq!(outcome.reason, None);
        assert_eq!(outcome.commits, 2);
        assert_eq!(outcome.min_score, Some(1.0));
        assert_eq!(outcome.concerns, 0);

        let kinds = built.sink.0.kinds();
        assert_eq!(kinds.first(), Some(&EventKind::SessionOpen));
        assert_eq!(kinds[1], EventKind::PlanCompiled);
        assert_eq!(kinds.last(), Some(&EventKind::SessionClose));
        assert_eq!(
            kinds.iter().filter(|k| **k == EventKind::NodeStart).count(),
            2
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == EventKind::NodeStop).count(),
            2
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == EventKind::LlmCall).count(),
            2
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == EventKind::HandoffRequest)
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == EventKind::HandoffAccept)
                .count(),
            1
        );

        // node.stop carries the disposition: research commits → "done" (twice total).
        let stops = built.sink.0.attrs_of(EventKind::NodeStop);
        assert!(stops.iter().all(|a| a.get_str("stop_kind") == Some("done")));
        // The accept names both ends of the edge.
        let accepts = built.sink.0.attrs_of(EventKind::HandoffAccept);
        assert_eq!(accepts[0].get_str("from"), Some("research"));
        assert_eq!(accepts[0].get_str("to"), Some("draft"));
        // session.close records the SCREAMING final state (tokens.md §2.1).
        assert_eq!(
            built.sink.0.attrs_of(EventKind::SessionClose)[0].get_str("final_state"),
            Some("COMPLETED")
        );

        // The WAL holds both commit points with the routed contract on the second.
        let wal = SessionWal::open(
            &built
                .root
                .join(".hesmos/sessions")
                .join(built.session_id.to_string())
                .join("checkpoint.db"),
        )
        .expect("reopen wal");
        assert_eq!(
            wal.commit_seqs(&built.session_id).expect("seqs"),
            vec![CommitSeq(1), CommitSeq(2)]
        );
        let commits = wal.commits(&built.session_id).expect("commits");
        assert_eq!(commits[0].node.as_str(), "research");
        assert!(
            commits[0].contract.is_none(),
            "the root node was not routed into"
        );
        assert_eq!(commits[1].node.as_str(), "draft");
        assert!(
            commits[1].contract.is_some(),
            "draft's checkpoint stores its handoff"
        );
    }

    /// The gate chain the runner walks: exactly the baseline pre/post refs per node,
    /// G0 in its own phase, in BTree (deterministic) order.
    #[test]
    fn gate_chain_is_baseline_pre_and_post_in_fixed_order() {
        let built = build(
            "chain",
            PLAN_YAML,
            EchoMode::Echo,
            PolicySet::default(),
            Some(ScriptedGates::all_pass()),
            Box::new(FakeBudget::new(0)),
        );
        let outcome = built.runner.run().expect("run");
        assert_eq!(outcome.final_state, SessionState::Completed);

        let gates = built.gates.as_ref().expect("scripted gates");
        let seen = gates.seen.lock().expect("lock").clone();
        // 2 nodes × (4 pre + 3 post) = 14 gate runs.
        assert_eq!(seen.len(), 14);
        let pre: Vec<&str> = seen
            .iter()
            .filter(|(_, p)| *p != RunnerPhase::Post)
            .map(|(r, _)| r.as_str())
            .collect();
        assert_eq!(
            pre,
            vec![
                "G0",
                "budget",
                "permission",
                "schema",
                "G0",
                "budget",
                "permission",
                "schema"
            ]
        );
        let post: Vec<&str> = seen
            .iter()
            .filter(|(_, p)| *p == RunnerPhase::Post)
            .map(|(r, _)| r.as_str())
            .collect();
        // BTree (lexicographic) order inside the union: done_criteria < rubric < schema.
        assert_eq!(
            post,
            vec![
                "done_criteria",
                "rubric",
                "schema",
                "done_criteria",
                "rubric",
                "schema"
            ]
        );
    }

    /// Budget suspend: the session suspends at the FIRST commit boundary past the
    /// line — one checkpoint exists (resume point), reason BUDGET_EXCEEDED, the
    /// budget.event(suspend) is in the log, and node.stop says escalate.
    #[test]
    fn budget_exceeded_suspends_at_a_commit_boundary() {
        // 700 tokens per call: after commit #1 the spend is ≥ 1000 → suspend.
        let built = build(
            "suspend",
            PLAN_YAML,
            EchoMode::Echo,
            PolicySet::default(),
            Some(ScriptedGates::all_pass()),
            Box::new(FakeBudget::new(700)),
        );
        let outcome = built.runner.run().expect("run");

        assert_eq!(outcome.final_state, SessionState::Suspended);
        assert_eq!(outcome.reason, Some(ReasonCode::BUDGET_EXCEEDED));
        assert_eq!(outcome.commits, 1, "the suspend checkpoint is commit #1");

        let kinds = built.sink.0.kinds();
        assert!(kinds.contains(&EventKind::BudgetEvent));
        let close = built.sink.0.attrs_of(EventKind::SessionClose);
        assert_eq!(close[0].get_str("final_state"), Some("SUSPENDED"));

        let wal = SessionWal::open(
            &built
                .root
                .join(".hesmos/sessions")
                .join(built.session_id.to_string())
                .join("checkpoint.db"),
        )
        .expect("reopen wal");
        assert_eq!(
            wal.commit_seqs(&built.session_id).expect("seqs"),
            vec![CommitSeq(1)]
        );
        let row = wal
            .session_row(&built.session_id)
            .expect("row")
            .expect("session");
        assert_eq!(row.state, "SUSPENDED");
    }

    /// Bounded provider failure: budget 2 → two retries granted, the third failure
    /// halts the session with the provider reason (exit 12 shape).
    #[test]
    fn provider_failures_retry_bounded_then_halt() {
        let policy = PolicySet {
            bounded_retry: 2,
            ..Default::default()
        };
        let built = build(
            "flaky",
            PLAN_YAML,
            EchoMode::Flaky(3),
            policy,
            Some(ScriptedGates::all_pass()),
            Box::new(FakeBudget::new(0)),
        );
        let outcome = built.runner.run().expect("run");

        assert_eq!(outcome.final_state, SessionState::Halted);
        assert_eq!(outcome.reason, Some(ReasonCode::PROVIDER_FAILURE));
        assert_eq!(outcome.commits, 0, "the failing node never committed");
        // Three llm attempts happened on `research` (1 + 2 retries) — the first node's
        // start exists, its stop is escalate, and nothing else started.
        assert_eq!(built.sink.0.attrs_of(EventKind::NodeStart).len(), 1);
        let stops = built.sink.0.attrs_of(EventKind::NodeStop);
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].get_str("stop_kind"), Some("escalate"));
    }

    /// The llm.call `provider` attr is the EXECUTOR's self-report, not a runner
    /// hardcode: a real adapter's identity (e.g. the FFI bridge's model_ref backing
    /// provider) must reach the trace verbatim (ml.md §6d decision 16 — "echo" used
    /// to be hardcoded at the emit site).
    #[test]
    fn llm_call_provider_is_the_executor_self_report() {
        struct BridgeDouble;
        impl StageExecutor for BridgeDouble {
            fn execute(&self, req: &StageRequest) -> Result<ExecutionReport, StageFailure> {
                Ok(ExecutionReport {
                    payload: serde_json::json!({ "node": req.node.as_str() }),
                    confidence: 0.9,
                    tokens_in: 1,
                    tokens_out: 1,
                    latency_ms: 0,
                    provider: "bridge-9".into(),
                    prompt_hash: None,
                    external_origin: None,
                })
            }
        }
        let built = build_with_executor(
            "provider-report",
            PLAN_YAML,
            Box::new(BridgeDouble),
            PolicySet::default(),
            Some(ScriptedGates::all_pass()),
            Box::new(FakeBudget::new(0)),
        );
        let outcome = built.runner.run().expect("run");
        assert_eq!(outcome.final_state, SessionState::Completed);

        let calls = built.sink.0.attrs_of(EventKind::LlmCall);
        assert!(!calls.is_empty(), "the plan ran at least one LLM turn");
        for attrs in &calls {
            assert_eq!(attrs.get_str("provider"), Some("bridge-9"));
        }
    }

    /// Post-gate Retry then Pass: the boundary counts as CONCERNS exactly once, and
    /// the retry did not duplicate node.start/stop.
    #[test]
    fn retry_then_pass_counts_one_concern() {
        // research: 4 pre pass; attempt0 post [schema p, done_criteria p, rubric r];
        // attempt1 post [p, p, p] → commit. draft: 4 pre + 3 post pass.
        let script = "pppp pp r ppp pppp ppp";
        let built = build(
            "concerns",
            PLAN_YAML,
            EchoMode::Echo,
            PolicySet::default(),
            Some(ScriptedGates::scripted(script)),
            Box::new(FakeBudget::new(0)),
        );
        let outcome = built.runner.run().expect("run");

        assert_eq!(outcome.final_state, SessionState::Completed);
        assert_eq!(outcome.concerns, 1);
        assert_eq!(
            outcome.min_score,
            Some(0.4),
            "the retry's score is the session min"
        );
        assert_eq!(outcome.commits, 2);
        assert_eq!(
            built.sink.0.attrs_of(EventKind::NodeStart).len(),
            2,
            "no extra starts"
        );
        assert_eq!(
            built.sink.0.attrs_of(EventKind::LlmCall).len(),
            3,
            "research ran twice"
        );
    }

    /// Bounded-retry exhaustion escalates: the failing gate re-records the failure as
    /// the escalation event, and the session FAILS with GATE_REJECT.
    #[test]
    fn retry_exhaustion_escalates_and_fails() {
        // research pre ×4 pass; post schema/done_criteria pass, rubric retry — three
        // times (attempts 0,1,2 with bounded_retry 2): third Retry exhausts.
        let script = "pppp ppr ppr ppr j";
        let built = build(
            "exhaust",
            PLAN_YAML,
            EchoMode::Echo,
            PolicySet::default(),
            Some(ScriptedGates::scripted(script)),
            Box::new(FakeBudget::new(0)),
        );
        let outcome = built.runner.run().expect("run");

        assert_eq!(outcome.final_state, SessionState::Failed);
        assert_eq!(outcome.reason, Some(ReasonCode::GATE_REJECT));
        assert_eq!(outcome.commits, 0);
        assert_eq!(
            outcome.min_score,
            Some(0.0),
            "the escalation re-run rejects at 0"
        );
        // The scripted conductor's 5th answer was the escalation re-run ('j').
        let gates = built.gates.as_ref().expect("scripted gates");
        assert_eq!(gates.seen.lock().expect("lock").len(), 4 + 3 + 3 + 3 + 1);
    }

    /// Loop-guard halt: max_handoffs 1 → the second edge's route halts the session
    /// (exit 11 shape), the first commit stands as evidence, no new node starts.
    #[test]
    fn handoff_cap_halts_the_session() {
        let policy = PolicySet {
            max_handoffs: 1,
            ..Default::default()
        };
        let root = temp_root("halt");
        let plan = crate::compile::parse_plan(CHAIN3_YAML).expect("plan");
        let sink = SinkHandle(Arc::new(RecordingSink::new()));
        let runner = Runner::open(
            RunnerConfig {
                root: root.clone(),
                policy: policy.clone(),
            },
            &plan,
            SessionId::generate(),
            7,
            BudgetEnvelope::default(),
            None,
            None,
            Box::new(EchoExecutor::new(EchoMode::Echo)),
            Box::new(GatesHandle(ScriptedGates::all_pass())),
            Box::new(FakeBudget::new(0)),
            accept_router("relay", policy.clone()),
            Box::new(FixedCache(false)),
            Box::new(sink.clone()),
        )
        .expect("open");
        let outcome = runner.run().expect("run");

        assert_eq!(outcome.final_state, SessionState::Halted);
        assert_eq!(outcome.reason, Some(ReasonCode::MAX_HANDOFFS));
        assert_eq!(outcome.commits, 1, "only `a` committed before the halt");
        let router_fails = sink.0.attrs_of(EventKind::GateFail);
        assert_eq!(router_fails.len(), 1);
        assert_eq!(router_fails[0].get_str("gate_id"), Some("router.v1"));
        assert_eq!(router_fails[0].get_str("reason_code"), Some("MAX_HANDOFFS"));
        // node b started (its boundary began) but node c never did.
        let starts = sink.0.attrs_of(EventKind::NodeStart);
        assert_eq!(
            starts.len(),
            2,
            "a and b started; c never starts after a halt"
        );
    }

    /// Route rejection: a raised confidence floor + low-confidence output → the router
    /// sends the contract back, the boundary fails with router.v1 gate.fail, and
    /// NOTHING commits (exit 20 shape via the router, not a gate).
    #[test]
    fn route_rejection_fails_the_session_without_committing() {
        let policy = PolicySet {
            min_confidence: 0.5,
            ..Default::default()
        };
        let built = build(
            "reject",
            PLAN_YAML,
            EchoMode::Reject,
            policy,
            Some(ScriptedGates::all_pass()),
            Box::new(FakeBudget::new(0)),
        );
        let outcome = built.runner.run().expect("run");

        assert_eq!(outcome.final_state, SessionState::Failed);
        assert_eq!(outcome.reason, Some(ReasonCode::GATE_REJECT));
        assert_eq!(outcome.commits, 0);
        // BoundedRetry grants (2) then ReturnToSender: three route attempts total.
        assert_eq!(built.sink.0.attrs_of(EventKind::HandoffRequest).len(), 3);
        let fails = built.sink.0.attrs_of(EventKind::GateFail);
        assert_eq!(fails.len(), 1);
        assert_eq!(fails[0].get_str("gate_id"), Some("router.v1"));
        assert_eq!(
            fails[0].get_f32("score"),
            Some(0.3),
            "the score IS the contract confidence"
        );
    }

    /// Replay forks: the derived session id is stable per (origin, at), the fork
    /// reuses the origin's cached responses byte-for-byte inside the window, and the
    /// fork's own responses/ is written for its (continuing) commits.
    #[test]
    fn fork_reuses_cached_responses_and_mints_a_deterministic_id() {
        // 1. origin run to completion.
        let origin = build(
            "fork-origin",
            PLAN_YAML,
            EchoMode::Echo,
            PolicySet::default(),
            Some(ScriptedGates::all_pass()),
            Box::new(FakeBudget::new(0)),
        );
        let origin_outcome = origin.runner.run().expect("origin run");
        assert_eq!(origin_outcome.final_state, SessionState::Completed);
        let origin_responses = origin
            .root
            .join(".hesmos/sessions")
            .join(origin.session_id.to_string())
            .join("responses");

        // 2. the fork id is a pure function of (origin, at).
        let sid_a = derive_fork_session_id(&origin.session_id, CommitSeq(1));
        let sid_b = derive_fork_session_id(&origin.session_id, CommitSeq(1));
        assert_eq!(
            sid_a, sid_b,
            "same origin+at → same fork id (S1 comparability)"
        );
        assert_ne!(
            sid_a, origin.session_id,
            "fork id differs from the origin (TYPE-5)"
        );
        assert_ne!(
            derive_fork_session_id(&origin.session_id, CommitSeq(2)),
            sid_a,
            "different fork point → different fork id"
        );

        // 3. run the fork at commit 1: commit #1 comes from the cache, #2 executes.
        let root = temp_root("fork-run");
        let plan = crate::compile::parse_plan(PLAN_YAML).expect("plan");
        let fork_session = Runner::open(
            RunnerConfig {
                root: root.clone(),
                policy: PolicySet::default(),
            },
            &plan,
            sid_a,
            42,
            BudgetEnvelope::default(),
            None,
            Some(ForkSource {
                origin: origin.session_id,
                at: CommitSeq(1),
                origin_responses: origin_responses.clone(),
            }),
            Box::new(EchoExecutor::new(EchoMode::Echo)),
            Box::new(GatesHandle(ScriptedGates::all_pass())),
            Box::new(FakeBudget::new(0)),
            accept_router("write the report", PolicySet::default()),
            Box::new(FixedCache(false)),
            Box::new(SinkHandle(Arc::new(RecordingSink::new()))),
        )
        .expect("fork open");
        assert_eq!(fork_session.handle().session_id, sid_a);
        assert_eq!(
            fork_session.handle().fork_of,
            Some((origin.session_id, CommitSeq(1))),
            "fork lineage stamped on the handle"
        );
        let fork_outcome = fork_session.run().expect("fork run");
        assert_eq!(fork_outcome.final_state, SessionState::Completed);
        assert_eq!(fork_outcome.commits, 2);

        // 4. the reused commit's cached envelope bytes are identical to the origin's.
        let origin_cache = std::fs::read(origin_responses.join("1.json")).expect("origin cache");
        let fork_cache = std::fs::read(
            root.join(".hesmos/sessions")
                .join(sid_a.to_string())
                .join("responses/1.json"),
        )
        .expect("fork cache");
        assert_eq!(
            origin_cache, fork_cache,
            "byte-level response reuse inside the window"
        );
    }

    /// External interrupt: the cooperative flag suspends the session with NO reason
    /// code (the CLI maps that to 130) and no budget event is emitted.
    #[test]
    fn interrupt_suspends_without_a_reason() {
        let built = build(
            "interrupt",
            PLAN_YAML,
            EchoMode::Echo,
            PolicySet::default(),
            Some(ScriptedGates::all_pass()),
            Box::new(FakeBudget::new(0)),
        );
        built.runner.interrupt();
        let outcome = built.runner.run().expect("run");

        assert_eq!(outcome.final_state, SessionState::Suspended);
        assert_eq!(outcome.reason, None, "an interrupt is not a reason code");
        assert_eq!(outcome.commits, 0);
        assert!(
            !built.sink.0.kinds().contains(&EventKind::BudgetEvent),
            "an interrupt is not a budget event"
        );
    }

    /// Fork id derivation is a pure hash function — pin the exact value so an
    /// accidental algorithm change is a visible event, not a silent S1 break.
    #[test]
    fn fork_id_derivation_is_pinned() {
        let origin = SessionId::from_u128(0xA11CE);
        let sid = derive_fork_session_id(&origin, CommitSeq(3));
        // Independent recomputation of the documented formula.
        let digest = canonical_sha256(&format!("fork:{origin}#3"));
        let mut bytes = [0u8; 16];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&digest.as_str()[i * 2..i * 2 + 2], 16).expect("hex");
        }
        assert_eq!(sid, SessionId::from_u128(u128::from_be_bytes(bytes)));
        // Well-formed ULID string (canonical 26 chars) — it must survive the wire.
        assert_eq!(sid.to_string().len(), 26);
    }

    // -- WP-P2a: SS-20 taint transitions and the SS-18 cache halt -----------------

    /// Gate conductor that lets every gate pass while capturing each request's
    /// input-envelope taint — the observation window on what a node's Pre boundary
    /// actually saw. The captured log is shared (Arc) so the test reads it after the
    /// runner consumed the conductor.
    struct TaintProbe(Arc<Mutex<Vec<(String, bool)>>>);
    impl GateConductor for TaintProbe {
        fn run_gate(&self, req: GateRun<'_>) -> GateVerdict {
            let clean = req.envelope_in.map(|e| e.taint.is_clean()).unwrap_or(true);
            self.0
                .lock()
                .expect("lock")
                .push((format!("{}:{}", req.node.as_str(), req.gate_ref), clean));
            GateVerdict::Pass { score: 1.0 }
        }
    }

    /// AC4 end to end: an External-mode node's output is Tainted, and the successor's
    /// Pre boundary receives a Tainted input envelope (merge + derive, both assembly
    /// paths). The clean root's boundary stays Clean — taint only follows data.
    #[test]
    fn external_output_marks_taint_through_the_chain() {
        let root = temp_root("taint-chain");
        let plan = crate::compile::parse_plan(PLAN_YAML).expect("plan parses");
        let log = Arc::new(Mutex::new(Vec::new()));
        let probe = TaintProbe(Arc::clone(&log));
        let sink = SinkHandle(Arc::new(RecordingSink::new()));
        let policy = PolicySet::default();
        let runner = Runner::open(
            RunnerConfig {
                root: root.clone(),
                policy: policy.clone(),
            },
            &plan,
            SessionId::generate(),
            42,
            BudgetEnvelope::default(),
            None,
            None,
            Box::new(EchoExecutor::new(EchoMode::External("web.search"))),
            Box::new(probe) as Box<dyn GateConductor>,
            Box::new(FakeBudget::new(0)),
            accept_router("write the report", policy),
            Box::new(FixedCache(false)),
            Box::new(sink),
        )
        .expect("open");

        let outcome = runner.run().expect("run");
        assert_eq!(outcome.final_state, SessionState::Completed);

        let seen = log.lock().expect("lock").clone();
        // research is the root: its Pre input (task only) is Clean...
        let research_pre = seen
            .iter()
            .find(|(k, _)| k == "research:G0")
            .expect("research pre gate ran");
        assert!(research_pre.1, "root boundary input must be Clean");
        // ...draft merges research's Tainted output — its Pre input CANNOT be Clean.
        let draft_pre = seen
            .iter()
            .find(|(k, _)| k == "draft:G0")
            .expect("draft pre gate ran");
        assert!(
            !draft_pre.1,
            "SS-20: successor input of an external output must be Tainted"
        );
    }

    /// Executor double counting calls — the zero-retry assertion's meter. The counter
    /// rides in an Arc so it survives the runner consuming the executor.
    struct CountingExecutor {
        inner: EchoExecutor,
        calls: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl StageExecutor for CountingExecutor {
        fn execute(&self, req: &StageRequest) -> Result<ExecutionReport, StageFailure> {
            self.calls.set(self.calls.get() + 1);
            self.inner.execute(req)
        }
    }

    /// SS-18 + exceptions §4 special row: a cache violation halts IMMEDIATELY —
    /// FAILED(GATE_REJECT) exit band, the gate_id="cache" violation event recorded,
    /// the turn void (no llm.call/meter row), and ZERO retries even though the
    /// bounded-retry budget is far from spent.
    #[test]
    fn cache_violation_halts_immediately_without_retry() {
        let root = temp_root("cache-halt");
        let plan = crate::compile::parse_plan(PLAN_YAML).expect("plan parses");
        let sink = SinkHandle(Arc::new(RecordingSink::new()));
        let policy = PolicySet {
            bounded_retry: 3,
            ..PolicySet::default()
        };
        let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let runner = Runner::open(
            RunnerConfig {
                root: root.clone(),
                policy: policy.clone(),
            },
            &plan,
            SessionId::generate(),
            42,
            BudgetEnvelope::default(),
            None,
            None,
            Box::new(CountingExecutor {
                inner: EchoExecutor::new(EchoMode::Echo),
                calls: std::rc::Rc::clone(&calls),
            }),
            Box::new(GatesHandle(ScriptedGates::all_pass())),
            Box::new(FakeBudget::new(0)),
            accept_router("write the report", policy),
            // Frozen session, every turn drifts — the sentinel violates on turn one.
            Box::new(FixedCache(true)),
            Box::new(sink.clone()),
        )
        .expect("open");

        let outcome = runner.run().expect("run");
        assert_eq!(outcome.final_state, SessionState::Failed);
        assert_eq!(outcome.reason, Some(ReasonCode::GATE_REJECT));

        // 재시도 0건: exactly ONE executor call across the whole session — the
        // bounded-retry loop (budget 3 above) is never entered on this failure.
        assert_eq!(
            calls.get(),
            1,
            "exceptions §4: no retry after a cache violation"
        );

        let kinds = sink.0.kinds();
        assert_eq!(
            kinds.iter().filter(|k| **k == EventKind::LlmCall).count(),
            0,
            "the violating turn is void — no llm.call, no meter row"
        );
        let fails = sink.0.attrs_of(EventKind::GateFail);
        assert_eq!(
            fails.len(),
            1,
            "exactly the cache violation, nothing retried"
        );
        assert_eq!(fails[0].get_str("gate_id"), Some("cache"));
        assert_eq!(fails[0].get_str("reason_code"), Some("GATE_REJECT"));
        // Immediate halt: the first node started, the second NEVER did, and the
        // session still closes (FAILED) on the record.
        assert_eq!(
            kinds.iter().filter(|k| **k == EventKind::NodeStart).count(),
            1,
            "the session stops at the violating node"
        );
        assert!(kinds.contains(&EventKind::SessionClose));
    }
}
