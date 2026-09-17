//! The FFI crate's own composition adapters — the session_run path's seam
//! implementations (D-7: binary crates cannot be depended on, so the CLI's
//! `hesmos/src/composition.rs` cannot be reused; these adapters re-implement the
//! same delegation over the same workspace crates — glue, no new judgment logic).
//!
//! The ONE deliberate difference from the CLI composition is [`PyBridgeExecutor`]:
//! the CLI runs the deterministic echo executor, the FFI bridge dispatches to the
//! Python callback registry (PY-3). Per the backend.md "P1e 후속" consensus, that
//! bridge passes MECHANICAL facts only and converts them to a runner ReasonCode in
//! exactly one place (the mapping table in [`PyBridgeExecutor::execute`]).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pyo3::Python;

use hesmos_budget::{
    BudgetEngine, Ledger, MeterEntry, MeterKind as LedgerMeterKind, SessionBudget, ThresholdVerdict,
};
use hesmos_core::{
    AgentRole, BudgetEnvelope, BudgetState, ContractJudgment, EventAttrs, EventKind, EventSink,
    GateVerdict, HandoffContract, PendingEvent, Plan, PolicySet, ReasonCode, SessionId, StageSpec,
    Taint, TeamId, canonical_sha256, external_import_verdict,
};
use hesmos_guard::{Gate, GateCtx, GatePhase, LeaseRegistry, instantiate, run_checked_with};
use hesmos_orchestrator::{
    BudgetConductor, ContractValidator, GateConductor, GateRun, MeterKind, RunnerPhase,
    SpendVerdict, StageExecutor, StageFailure, StageRequest,
};
use hesmos_trace::EventLog;

use crate::callbacks;

// ---------------------------------------------------------------------------
// Contract validation + gates — same delegation as the CLI composition root
// ---------------------------------------------------------------------------

/// Delegates to the guard's field-absence matrix validator (TRAIT-4 step 1).
pub struct GuardValidator;

impl ContractValidator for GuardValidator {
    fn validate(
        &self,
        contract: &HandoffContract,
        session_task: &str,
        confidence_floor: f32,
    ) -> ContractJudgment {
        hesmos_guard::validate(contract, session_task, confidence_floor)
    }
}

/// Runs real guard gates and owns the [`LeaseRegistry`] (a guard type that cannot
/// cross the seam — the adapter lends `&` to each `GateCtx`).
pub struct GuardGates {
    leases: LeaseRegistry,
    /// The session's judge ledger (US-26) — drained at POST phase so a judged
    /// node's gate events carry the `judge.*` metadata (W3-5 optional attrs,
    /// sanctioned on GatePass and GateFail alike). `None` = rules-only session.
    judge: Option<JudgeLedger>,
}

impl GuardGates {
    pub fn new() -> Self {
        Self {
            leases: LeaseRegistry::new(),
            judge: None,
        }
    }

    /// The judging composition: drains the executor's judge metadata into gate
    /// events. Created once per session in session_open, sharing the executor's
    /// [`JudgeLedger`] Arc.
    pub fn with_judge(leases: LeaseRegistry, judge: JudgeLedger) -> Self {
        Self {
            leases,
            judge: Some(judge),
        }
    }
}

impl Default for GuardGates {
    fn default() -> Self {
        Self::new()
    }
}

impl GateConductor for GuardGates {
    fn run_gate(&self, req: GateRun<'_>) -> GateVerdict {
        // Unknown gate references are a composition invariant: the plan compiled, so a
        // ref `instantiate` cannot name is a schema bug — panic, never pass.
        let deps = hesmos_guard::GateDeps {
            session_task: req.session_task,
            spawn_token_floor: req.spawn_token_floor,
            grantor_profile: req.grantor_profile,
        };
        let gate: Box<dyn Gate> = instantiate(req.gate_ref, &deps)
            .unwrap_or_else(|| panic!("unknown gate reference `{}`", req.gate_ref));
        let phase = match req.phase {
            RunnerPhase::Pre => GatePhase::Pre,
            RunnerPhase::Post => GatePhase::Post,
            RunnerPhase::G0 => GatePhase::G0,
        };
        let mut ctx = GateCtx::base(
            req.session,
            req.node.clone(),
            phase,
            req.policy,
            req.budget_state,
            &self.leases,
        );
        ctx.envelope_in = req.envelope_in;
        ctx.envelope_out = req.envelope_out;
        ctx.contract = req.contract;
        ctx.attempt = req.attempt;
        // run_checked_with emits gate.pass/gate.fail on the RUNNER's sink (lent through
        // GateRun) — the single-chain invariant (SS-09 rule 3) holds in this composition
        // too, even though orchestrator cannot name hesmos_guard (D-2).
        //
        // US-26: a judged node's POST gates carry the judge metadata — drained here
        // so the attrs exist ONLY when a judgment actually ran (an unjudged node's
        // events stay clean; a completed judgment cannot skip its record). PRE/G0
        // gates never carry judge attrs: the judgment is about the OUTPUT.
        match self
            .judge
            .as_ref()
            .filter(|_| req.phase == RunnerPhase::Post)
            .map(|ledger| ledger.attrs_for(req.node.as_str()))
        {
            None => run_checked_with(gate.as_ref(), &ctx, req.sink, req.extra),
            Some(judge_attrs) => {
                let mut merged: Vec<(&str, serde_json::Value)> =
                    req.extra.iter().map(|(k, v)| (*k, v.clone())).collect();
                merged.extend(judge_attrs.iter().map(|(k, v)| (k.as_str(), v.clone())));
                run_checked_with(gate.as_ref(), &ctx, req.sink, &merged)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Budget — meter into the session ledger inside checkpoint.db (CLI parity)
// ---------------------------------------------------------------------------

/// Meters into the session ledger and answers with threshold verdicts. Structural
/// twin of the CLI's `BudgetMeter`; the panic-on-write-failure policy is inherited:
/// a lost ledger row would corrupt US-20 AC3 (ledger sums == event sums).
pub struct BudgetMeter {
    ledger: Ledger,
    engine: RefCell<BudgetEngine>,
    team_id: Option<String>,
    /// Agent-scope spend within this session, per role.
    agent_spent: RefCell<BTreeMap<String, u64>>,
    /// ponytail: the TEAM scope counts THIS session's contribution only — true
    /// cross-session sums need the team's other ledgers (same limitation the CLI
    /// composition documents). upgrade trigger: any multi-session feature.
    team_spent: RefCell<u64>,
}

impl BudgetMeter {
    pub fn open(
        session_id: &SessionId,
        db_path: &Path,
        budget: &BudgetEnvelope,
        team_id: Option<&TeamId>,
    ) -> Result<Self, hesmos_budget::LedgerError> {
        let ledger = Ledger::open(session_id, &db_path.display().to_string())?;
        let frozen = SessionBudget::freeze(budget.clone(), *session_id, team_id.cloned());
        Ok(Self {
            ledger,
            engine: RefCell::new(BudgetEngine::new(frozen)),
            team_id: team_id.map(|t| t.to_string()),
            agent_spent: RefCell::new(BTreeMap::new()),
            team_spent: RefCell::new(0),
        })
    }

    fn session_spent(&self) -> u64 {
        self.ledger.session_totals().map(|t| t.total()).unwrap_or(0)
    }
}

impl BudgetConductor for BudgetMeter {
    fn snapshot(&self, role: &AgentRole) -> BudgetState {
        self.engine.borrow().budget_state(
            self.session_spent(),
            *self.team_spent.borrow(),
            role,
            *self.agent_spent.borrow().get(role.as_str()).unwrap_or(&0),
        )
    }

    fn meter(
        &self,
        role: &AgentRole,
        kind: MeterKind,
        tokens_in: u64,
        tokens_out: u64,
    ) -> SpendVerdict {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_millis() as i64;
        let entry = MeterEntry {
            team_id: self.team_id.clone(),
            agent_role: role.as_str().to_string(),
            kind: match kind {
                MeterKind::Llm => LedgerMeterKind::Llm,
                MeterKind::Tool => LedgerMeterKind::Tool,
            },
            tokens_in,
            tokens_out,
            // Token-centric metering: cost stays None until a unit table exists (ERD §7).
            cost_usd: None,
        };
        if self.ledger.record(entry, ts).is_err() {
            panic!("metering ledger write failed — the budget evidence would diverge");
        }
        *self
            .agent_spent
            .borrow_mut()
            .entry(role.as_str().to_string())
            .or_insert(0) += tokens_in + tokens_out;
        *self.team_spent.borrow_mut() += tokens_in + tokens_out;

        let verdict = self.engine.borrow_mut().evaluate(
            self.session_spent(),
            *self.team_spent.borrow(),
            role,
            *self.agent_spent.borrow().get(role.as_str()).unwrap_or(&0),
        );
        match verdict {
            ThresholdVerdict::Clear => SpendVerdict::Clear,
            ThresholdVerdict::Warn {
                spent, remaining, ..
            } => SpendVerdict::Warn { spent, remaining },
            ThresholdVerdict::Exceeded { .. } => SpendVerdict::Exceeded,
        }
    }
}

/// The read-only status view (`budget_status`): rebuilds the frozen engine and reads
/// the ledger's own totals. The neutral role (`""`) never appears in an envelope's
/// `agent_max_tokens`, so the agent scope reports uncapped — "no role selected".
/// Reusing `budget_state` (the exact function the gates see) keeps ONE truth.
pub fn budget_state_snapshot(
    session_id: &SessionId,
    db_path: &Path,
    budget: &BudgetEnvelope,
    team_id: Option<&TeamId>,
) -> Result<BudgetState, hesmos_budget::LedgerError> {
    let ledger = Ledger::open(session_id, &db_path.display().to_string())?;
    let totals = ledger.session_totals()?;
    let engine = BudgetEngine::new(SessionBudget::freeze(
        budget.clone(),
        *session_id,
        team_id.cloned(),
    ));
    Ok(engine.budget_state(totals.total(), 0, &AgentRole::new(""), 0))
}

// ---------------------------------------------------------------------------
// Cache sentinel — the SS-18 core-half adapter (same shape as the CLI's)
// ---------------------------------------------------------------------------

/// Verifies each turn's prompt hash against the session's frozen system-prompt
/// invariant (the guard's cache judgment behind the seam).
///
/// ponytail: W5 has no system prompt — the frozen invariant stays `None`, so every
/// turn verifies Stable. upgrade trigger: WP-P2d's prompt builder (PY-7) lands → the
/// composition root freezes the builder's SS-04 hash at session start and every LLM
/// turn becomes a verified obligation.
pub struct CacheSentinel {
    pub frozen: Option<hesmos_guard::PromptInvariant>,
}

impl hesmos_orchestrator::CacheConductor for CacheSentinel {
    fn verify_turn(
        &self,
        reported: Option<&hesmos_core::Sha256Hex>,
    ) -> hesmos_orchestrator::CacheVerdict {
        match hesmos_guard::verify_turn(self.frozen.as_ref(), reported) {
            hesmos_guard::CacheJudgment::Stable => hesmos_orchestrator::CacheVerdict::Stable,
            hesmos_guard::CacheJudgment::Violated => hesmos_orchestrator::CacheVerdict::Violated,
        }
    }
}

// ---------------------------------------------------------------------------
// The Python bridge executor — PY-3 dispatch behind the StageExecutor seam
// ---------------------------------------------------------------------------

/// Per-session judge metadata awaiting its gate event (US-26, W3-5 seat). The
/// executor records a judged node's `judge.*` attrs here after its reply; the
/// gate conductor drains them when the node's POST gates fire — the ONLY path a
/// judgment can take, so a completed verdict cannot go unrecorded (SS-22 rule 3).
/// Shared like [`SharedLog`]: one Arc, two seam objects, no second channel.
type JudgeAttrs = Vec<(String, serde_json::Value)>;

#[derive(Default, Clone)]
pub struct JudgeLedger(Arc<Mutex<BTreeMap<String, JudgeAttrs>>>);

impl JudgeLedger {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, node: &str, attrs: Vec<(String, serde_json::Value)>) {
        self.0
            .lock()
            .expect("judge ledger poisoned")
            .insert(node.to_string(), attrs);
    }

    /// The judged node's attrs, CLONED not removed: every post gate of the node
    /// records the same judgment (a plan can carry several), and the next
    /// attempt's `record` overwrites the entry. The ledger dies with the
    /// session's Arc — no cleanup needed.
    fn attrs_for(&self, node: &str) -> Vec<(String, serde_json::Value)> {
        self.0
            .lock()
            .expect("judge ledger poisoned")
            .get(node)
            .cloned()
            .unwrap_or_default()
    }
}

/// Dispatches node work to the registered Python provider callback. Holds the
/// compiled plan's stage specs because [`StageRequest`] carries identity facts only
/// (no temperature/max_tokens) — the narrow-prompt data lives in the profile.
pub struct PyBridgeExecutor {
    stages: BTreeMap<String, StageSpec>,
    /// The session's byte-stable system prompt (SS-18 rule 1) — captured once at
    /// open and immutable for the executor's lifetime, so a frozen conductor and
    /// the turns it verifies can never disagree through this type.
    system_prompt: Option<String>,
    /// `sha256(canonical(system_prompt))` — frozen into the cache conductor at
    /// open and reported VERBATIM on every turn (S3 per-turn verification).
    prompt_hash: Option<hesmos_core::Sha256Hex>,
    /// The session's ONE trace log, shared with the runner's sink. The EventLog
    /// hash chain must never have two independent handles on the same file (they
    /// would fork the chain); the Arc keeps runner and executor on a single
    /// `Mutex<LogState>` so tool.call events interleave into the same sequence.
    log: SharedLog,
    /// The session's judge, snapshotted at OPEN (US-26): a judge registered
    /// AFTER this session opened never leaks into it. `None` = rules-only
    /// session (US-26 AC3 — the LLM judge is optional, never default).
    judge: Option<(callbacks::JudgeRegistration, JudgeLedger)>,
}

impl PyBridgeExecutor {
    pub fn new(
        plan: &Plan,
        system_prompt: Option<String>,
        log: SharedLog,
        judge: Option<(callbacks::JudgeRegistration, JudgeLedger)>,
    ) -> Self {
        let prompt_hash = system_prompt.as_ref().map(canonical_sha256);
        Self {
            stages: plan
                .stages
                .iter()
                .map(|s| (s.id.as_str().to_string(), s.clone()))
                .collect(),
            system_prompt,
            prompt_hash,
            log,
            judge,
        }
    }
}

impl StageExecutor for PyBridgeExecutor {
    fn execute(
        &self,
        req: &StageRequest,
    ) -> Result<hesmos_orchestrator::ExecutionReport, StageFailure> {
        // The executor only ever runs inside a pyfunction call (session_run), where the
        // GIL is held; try_attach returning None would mean a non-Python thread reached
        // here — a composition bug reported as a provider failure, never a panic.
        Python::try_attach(|py| self.execute_attached(py, req)).ok_or_else(|| StageFailure {
            reason_code: ReasonCode::PROVIDER_FAILURE,
            message: "internal: Python interpreter not attached on the executor thread".into(),
        })?
    }
}

impl PyBridgeExecutor {
    fn execute_attached(
        &self,
        py: pyo3::Python<'_>,
        req: &StageRequest,
    ) -> Result<hesmos_orchestrator::ExecutionReport, StageFailure> {
        // Wave nodes come from the compiled plan, so the lookup cannot miss; a miss
        // would mean the runner and the plan diverged — treat it as a contract bug.
        let spec = self
            .stages
            .get(req.node.as_str())
            .ok_or_else(|| StageFailure {
                reason_code: ReasonCode::PROVIDER_FAILURE,
                message: format!(
                    "internal: node `{}` absent from the compiled stage map",
                    req.node
                ),
            })?;
        let model = req.model.as_str();
        // Unregistered model_ref = evidence-preserving failure (SS-16 rule 2): the
        // session goes through bounded retry and halts with PROVIDER_FAILURE rather
        // than synthesising a reply.
        let callback = callbacks::lookup_provider(py, model).ok_or_else(|| StageFailure {
            reason_code: ReasonCode::PROVIDER_FAILURE,
            message: format!(
                "no provider registered for model `{model}` — register one with @core.provider(\"{model}\")"
            ),
        })?;

        let request = build_llm_request(self.system_prompt.as_deref(), spec, req);
        let started = Instant::now();
        let reply =
            callbacks::call(py, &callback, &request).map_err(mechanical_to_stage_failure)?;
        validate_reply_shape(&reply).map_err(|message| {
            mechanical_to_stage_failure(callbacks::CallbackFailure {
                kind: "LlmReplySchema".into(),
                message,
            })
        })?;
        let latency_ms = started.elapsed().as_millis() as u64;

        // Shape check guarantees these casts; unwrap_or(0) is unreachable belt-and-braces.
        let tokens_in = reply["tokens_in"].as_u64().unwrap_or(0);
        let tokens_out = reply["tokens_out"].as_u64().unwrap_or(0);

        // WP-P2e tool dispatch (single round): every gate-passed tool_call in the
        // reply goes to its registered callback here. The executor NEVER re-judges
        // permissions (SS-19 rule 3 — the permission gate already ran pre-execute);
        // its boundary jobs are mechanical dispatch, ToolResult shape validation,
        // and the SS-20 rule 1 taint verdict (see dispatch_tools).
        let empty = Vec::new();
        let calls = reply
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        let (tool_results, external_origin) = self.dispatch_tools(py, req, calls);

        // US-26: the optional quality judge runs AFTER the reply (and its tool
        // round) on the node output. Its tokens are folded into this turn's
        // llm.call accounting (SS-17 rule 3 — every LLM call counts, judge
        // included); its attrs go to the ledger for the POST gates to record.
        // A judge FAILURE degrades to rules-only (no attrs, payload records the
        // error) — the judge is optional, so unlike a provider failure it must
        // not fail the session.
        let mut judge_tokens = (0u64, 0u64);
        let mut judge_error: Option<String> = None;
        if let Some((registration, ledger)) = &self.judge {
            match run_judge(py, registration, reply["content"].as_str().unwrap_or("")) {
                Ok(result) => {
                    judge_tokens = (result.0, result.1);
                    ledger.record(
                        req.node.as_str(),
                        vec![
                            (
                                "judge.prompt_hash".to_string(),
                                serde_json::json!(registration.prompt_hash),
                            ),
                            (
                                "judge.temperature".to_string(),
                                serde_json::json!(registration.temperature),
                            ),
                            (
                                "judge.model_version".to_string(),
                                serde_json::json!(registration.model_version),
                            ),
                        ],
                    );
                }
                Err(message) => judge_error = Some(message),
            }
        }

        // Payload = mechanical facts of the call: what was asked, what came back,
        // and what every dispatched tool returned (or why it was rejected).
        let payload = serde_json::json!({
            "task": req.task,
            "attempt": req.attempt,
            "input": req.node_input,
            "reply": {
                "content": reply["content"],
                "tool_calls": reply.get("tool_calls").cloned().unwrap_or(serde_json::Value::Array(vec![])),
            },
            "tool_results": tool_results,
        });
        let payload = match judge_error {
            Some(message) => {
                let mut p = payload;
                p["judge_error"] = serde_json::json!(message);
                p
            }
            None => payload,
        };
        // confidence is fixed at 1.0: the PY-3 callback contract carries no confidence
        // signal, and inventing one would fabricate quality data. Quality judgment is
        // the gates' job (rubric/done_criteria), not the transport's.
        // prompt_hash: the frozen system prompt's hash, reported on EVERY turn —
        // a frozen conductor plus a turn reporting no hash is itself a violation
        // (guard::verify_turn fail-closed, SS-18 rule 2).
        Ok(hesmos_orchestrator::ExecutionReport {
            payload,
            confidence: 1.0,
            tokens_in: tokens_in + judge_tokens.0,
            tokens_out: tokens_out + judge_tokens.1,
            latency_ms,
            // The serving provider is the model_ref the bridge dispatched to —
            // the runner never names one itself (the echo hardcode is gone).
            provider: req.model.as_str().to_string(),
            prompt_hash: self.prompt_hash.clone(),
            // Declared when an accepted tool result was Tainted: the runner then
            // marks the output envelope (the sanctioned Clean→Tainted path), so a
            // tainted read propagates to every downstream envelope (S5).
            external_origin,
        })
    }

    /// Dispatches one round of tool_calls. Each call is dispatched independently;
    /// a rejection (unregistered tool, callback exception, malformed ToolResult,
    /// unmarked external import) is recorded as tool.call(ok=false) and a
    /// structured reason in the payload — it does NOT fail the session (the
    /// W3-confirmed path: rejections are evidence, not crashes). Emitted events
    /// carry the TYPE-3-required attrs (tool_name/ok/latency_ms/actor).
    fn dispatch_tools(
        &self,
        py: Python<'_>,
        req: &StageRequest,
        calls: &[serde_json::Value],
    ) -> (Vec<serde_json::Value>, Option<String>) {
        let mut results = Vec::with_capacity(calls.len());
        let mut external_origin: Option<String> = None;
        for call in calls {
            let started = Instant::now();
            let emit = |ok: bool| {
                // TYPE-3: the boundary refuses a tool.call missing any required attr,
                // so all four are set on every path (ok=false carries them too).
                let latency_ms = started.elapsed().as_millis() as u64;
                self.log.emit(
                    PendingEvent::new(
                        EventKind::ToolCall,
                        Some(req.node.clone()),
                        EventAttrs::new()
                            .set("tool_name", tool_name(call).unwrap_or_default())
                            .set("ok", ok)
                            .set("latency_ms", latency_ms)
                            .set("actor", "executor"),
                    )
                    .expect("tool.call attrs are complete by construction"),
                );
            };
            let outcome = self.dispatch_one(py, call);
            match outcome {
                Ok(result) => {
                    // First tainted origin wins (the runner marks a single first
                    // source — first-mark-wins, matching the assembly's rule).
                    if let Some(origin) = tainted_origin(&result)
                        && external_origin.is_none()
                    {
                        external_origin = Some(origin);
                    }
                    emit(true);
                    results.push(result);
                }
                Err(reason) => {
                    emit(false);
                    results.push(serde_json::json!({
                        "call_id": call.get("id").cloned().unwrap_or(serde_json::Value::Null),
                        "tool": tool_name(call).unwrap_or_default(),
                        "ok": false,
                        "reason": reason,
                    }));
                }
            }
        }
        (results, external_origin)
    }

    /// One tool_call → callback → ToolResult, with the boundary judgments:
    /// shape validation, then core's external_import_verdict (SS-20 rule 1).
    /// The verdict consumes the REGISTRATION-TIME externalness declaration —
    /// an external tool returning a Clean result is an unmarked import and the
    /// result is rejected (the executor never "fixes" a missing mark itself).
    fn dispatch_one(
        &self,
        py: Python<'_>,
        call: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let obj = call.as_object().ok_or("tool_call must be an object")?;
        // call_id → ToolResult.call_id pairing is the callback's own convention (the
        // mirror requires the field); the executor checks presence, not pairing.
        if !obj.get("id").is_some_and(|v| v.is_string()) {
            return Err("tool_call.id must be a string".into());
        }
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or("tool_call.name must be a string")?;
        if !obj.get("arguments").is_some_and(|v| v.is_object()) {
            return Err(format!("tool_call `{name}` arguments must be an object"));
        }
        let callback = callbacks::lookup_tool(py, name).ok_or_else(|| {
            format!(
                "no callback registered for tool `{name}` — register one with @core.tool(\"{name}\")"
            )
        })?;
        let result = callbacks::call(py, &callback, call)
            .map_err(|f| format!("tool callback failed ({}): {}", f.kind, f.message))?;
        let taint = validate_tool_result(name, &result)?;
        external_import_verdict(callbacks::is_external_tool(name), &taint, name).map_err(
            |unmarked| {
                format!(
                    "unmarked external import: tool `{}` declared external but returned a Clean result",
                    unmarked.origin
                )
            },
        )?;
        Ok(result)
    }
}

/// Runs the judge bridge on one node output → (tokens_in, tokens_out). The
/// bridge's reply shape is mechanical: {verdict: str, score: f64, tokens_in:
/// u64, tokens_out: u64}. The verdict/score stay in the payload's evidence —
/// the attrs the gates record are the registration-time metadata only (W3-5
/// seat: judge.verdict on GateFail is a runner/guard verdict-aware emission,
/// out of W5 scope; see ml.md §6g).
fn run_judge(
    py: Python<'_>,
    registration: &callbacks::JudgeRegistration,
    output: &str,
) -> Result<(u64, u64), String> {
    let result = callbacks::call(py, &registration.callback, &serde_json::json!(output))
        .map_err(|f| format!("judge callback failed ({}): {}", f.kind, f.message))?;
    let obj = result
        .as_object()
        .ok_or_else(|| "judge reply must be an object".to_string())?;
    if !obj.get("verdict").is_some_and(|v| v.is_string()) {
        return Err("judge reply.verdict must be a string".into());
    }
    if !obj.get("score").is_some_and(|v| v.is_number()) {
        return Err("judge reply.score must be a number".into());
    }
    for key in ["tokens_in", "tokens_out"] {
        if !obj.get(key).is_some_and(|v| v.is_u64()) {
            return Err(format!("judge reply.{key} must be a non-negative integer"));
        }
    }
    let tokens_in = obj["tokens_in"].as_u64().unwrap_or(0);
    let tokens_out = obj["tokens_out"].as_u64().unwrap_or(0);
    Ok((tokens_in, tokens_out))
}

/// Mechanical ToolResult shape check (the PY-4 mirror contract): call_id str,
/// content str, taint a core-shaped marking. A callback that omits `taint` is
/// rejected HERE — PT-12: an unmarked result cannot cross the boundary, and the
/// executor never infers a mark the tool author did not declare.
fn validate_tool_result(tool: &str, result: &serde_json::Value) -> Result<Taint, String> {
    let obj = result
        .as_object()
        .ok_or_else(|| format!("tool `{tool}` result must be an object (ToolResult shape)"))?;
    if !obj.get("call_id").is_some_and(|v| v.is_string()) {
        return Err(format!("tool `{tool}` result.call_id must be a string"));
    }
    if !obj.get("content").is_some_and(|v| v.is_string()) {
        return Err(format!("tool `{tool}` result.content must be a string"));
    }
    let taint = obj.get("taint").ok_or_else(|| {
        format!(
            "tool `{tool}` result carries no taint marking (PT-12 — every ToolResult is marked)"
        )
    })?;
    match taint.get("kind").and_then(|k| k.as_str()) {
        Some("Clean") => Ok(Taint::Clean),
        Some("Tainted") => {
            let origin = taint
                .get("source")
                .and_then(|s| s.get("origin"))
                .and_then(|o| o.as_str())
                .ok_or_else(|| {
                    format!("tool `{tool}` Tainted result must carry source.origin (TaintSource)")
                })?;
            Ok(Taint::Tainted {
                source: hesmos_core::TaintSource {
                    origin: origin.to_string(),
                },
            })
        }
        _ => Err(format!(
            "tool `{tool}` result.taint must be {{kind: Clean}} or {{kind: Tainted, source: {{origin}}}}"
        )),
    }
}

/// The `tool_call.name` when well-formed (used for event attrs; empty string for a
/// malformed call — the rejection record in the payload carries the full reason).
fn tool_name(call: &serde_json::Value) -> Option<&str> {
    call.get("name").and_then(|v| v.as_str())
}

/// The taint source origin of an ACCEPTED tool result (its `taint` field), when
/// Tainted — the executor's external_origin declaration for the runner.
fn tainted_origin(result: &serde_json::Value) -> Option<String> {
    let taint = result.get("taint")?;
    if taint.get("kind").and_then(|k| k.as_str()) != Some("Tainted") {
        return None;
    }
    taint
        .get("source")
        .and_then(|s| s.get("origin"))
        .and_then(|o| o.as_str())
        .map(str::to_string)
}

/// THE reason-mapping table (backend.md consensus b — exactly one site): every
/// mechanical callback failure maps to PROVIDER_FAILURE. The table has one entry
/// today because the Python callback vocabulary has no timeout/deadline signal yet;
/// when one lands (P2e), a TIMEOUT row is added HERE, not at the call sites.
fn mechanical_to_stage_failure(f: callbacks::CallbackFailure) -> StageFailure {
    StageFailure {
        reason_code: ReasonCode::PROVIDER_FAILURE,
        message: format!("provider callback failed ({}): {}", f.kind, f.message),
    }
}

/// The narrow LLM request built from the stage profile + the request's mechanical
/// facts. tools_schema is empty because the registration surface (PY-3) carries no
/// tool-schema metadata yet — narrow toolsets enter with WP-P2e/MCP. Breakpoint
/// placement follows the PromptBuilder's algorithm for the single-turn shape:
/// stable = the byte-stable system prefix (1 message when the session has a
/// prompt), context = none (no history yet), volatile = the uncached tail end.
/// profile.resource_limits.timeout_ms is unused for now: no executor-side deadline
/// enforcement exists yet (P2e).
fn build_llm_request(
    system_prompt: Option<&str>,
    spec: &StageSpec,
    req: &StageRequest,
) -> serde_json::Value {
    let mut messages = Vec::new();
    if let Some(prompt) = system_prompt {
        messages.push(serde_json::json!({"role": "system", "content": prompt}));
    }
    messages.push(serde_json::json!({
        "role": "user",
        "content": format!("{}\n\n{}", req.task, req.node_input),
    }));
    let total = messages.len() as u64;
    let stable = u64::from(system_prompt.is_some());
    serde_json::json!({
        "messages": messages,
        "temperature": spec.profile.temperature,
        "max_tokens": spec.profile.resource_limits.max_tokens,
        "tools_schema": [],
        "cache_breakpoints": {"stable": stable, "context": 0, "volatile": total},
    })
}

/// Mechanical PY-3 reply-shape check (LlmReply contract): content str, tokens_in/out
/// non-negative ints, tool_calls a list when present, provider_meta an object when
/// present. Defaults are NOT filled in — a callback that omits a required field is a
/// contract violation (SS-17 rule 3: missing tokens must fail the session, not zero).
fn validate_reply_shape(reply: &serde_json::Value) -> Result<(), String> {
    let Some(obj) = reply.as_object() else {
        return Err("reply must be an object (LlmReply shape)".into());
    };
    if !obj.get("content").is_some_and(|v| v.is_string()) {
        return Err("reply.content must be a string".into());
    }
    if !obj.contains_key("tokens_in") || !obj.contains_key("tokens_out") {
        return Err(
            "reply.tokens_in/tokens_out are required (SS-17 rule 3 — llm.call accounting source)"
                .into(),
        );
    }
    for key in ["tokens_in", "tokens_out"] {
        match obj.get(key) {
            Some(v) if v.is_u64() => {}
            _ => return Err(format!("reply.{key} must be a non-negative integer")),
        }
    }
    if let Some(calls) = obj.get("tool_calls")
        && !calls.is_array()
    {
        return Err("reply.tool_calls must be a list".into());
    }
    if let Some(meta) = obj.get("provider_meta")
        && !meta.is_object()
    {
        return Err("reply.provider_meta must be an object".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Policy / paths — the session_open inputs (CLI parity)
// ---------------------------------------------------------------------------

/// The composed policy: charter defaults, overridden by `HESMOS_POLICY_YAML` (inline
/// YAML) when set — same env contract as the CLI so one variable rules both surfaces.
/// A broken policy is a usage error, never silently ignored.
pub fn load_policy(py: pyo3::Python<'_>) -> pyo3::PyResult<PolicySet> {
    match std::env::var("HESMOS_POLICY_YAML") {
        Ok(text) => hesmos_guard::parse_str(&text).map_err(|e| {
            crate::marshal::raise_hesmos(
                py,
                "FfiError",
                "Ffi",
                "FFI-SCHEMA",
                format!("HESMOS_POLICY_YAML parse failed: {e}"),
                "fix the inline policy YAML (same surface as the CLI)",
            )
        }),
        Err(_) => Ok(PolicySet::default()),
    }
}

/// Session artifact paths — `<root>/.hesmos/sessions/<id>/` (trace crate layout),
/// same shape as the CLI's `session_paths`.
pub fn session_paths(root: &Path, session_id: &SessionId) -> (PathBuf, PathBuf, PathBuf) {
    let dir = root
        .join(".hesmos")
        .join("sessions")
        .join(session_id.to_string());
    (
        dir.clone(),
        dir.join("events.jsonl"),
        dir.join("checkpoint.db"),
    )
}

/// The events sink: the session's [`EventLog`] alone. The CLI tee adds console
/// progress because CLI-1 promises it; the FFI surface promises nothing on stdout,
/// so the log stays the single sink.
pub fn event_log(
    root: &Path,
    session_id: &SessionId,
) -> Result<EventLog, hesmos_trace::TraceError> {
    EventLog::open(root, session_id)
}

/// One [`EventLog`] shared by the runner's sink and the tool-dispatching executor.
/// The hash chain forbids a second handle on the same file (each handle keeps its
/// own `Mutex<LogState>` and would fork seq/prev_hash) — so the executor holds this
/// Arc wrapper and both funnel through the ONE chain state.
pub struct SharedLog(Arc<EventLog>);

impl SharedLog {
    pub fn new(log: EventLog) -> Self {
        Self(Arc::new(log))
    }

    /// A second handle for the same chain (executor-side tool.call emission).
    pub fn clone_handle(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl EventSink for SharedLog {
    /// Delegates to the EventLog's own infallible PORT-1 emit (append failure is
    /// fatal to the evidence chain — the panic contract is the EventLog's).
    fn emit(&self, e: PendingEvent) -> hesmos_core::TraceEvent {
        self.0.emit(e)
    }
}

/// Applies the §6.3 task rule to a raw plan value: the explicit task fills an
/// absent/blank `task`; both-present-and-different is a schema violation (one goal,
/// two spellings would silently fork goal_original). Returns (merged plan, task).
pub fn merge_task(
    py: pyo3::Python<'_>,
    mut plan: serde_json::Value,
    task: Option<&str>,
) -> pyo3::PyResult<(serde_json::Value, String)> {
    let existing = plan
        .get("task")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let resolved = match (existing.as_deref(), task) {
        (Some(from_plan), Some(from_call)) if from_plan.trim() == from_call.trim() => {
            from_call.to_string()
        }
        (Some(from_plan), Some(from_call)) => {
            return Err(crate::marshal::schema_violation(
                py,
                format!(
                    "plan.task and the task argument disagree: {from_plan:?} vs {from_call:?} — pass one of them"
                ),
            ));
        }
        (Some(from_plan), None) if !from_plan.trim().is_empty() => from_plan.to_string(),
        (Some(from_plan), None) => {
            return Err(crate::marshal::schema_violation(
                py,
                format!("plan.task is blank and no task argument was given ({from_plan:?})"),
            ));
        }
        (None, Some(from_call)) if !from_call.trim().is_empty() => {
            // Set before parse_plan so CE-05 (MissingGoalOriginal) never fires on a
            // §6.3-shaped plan — the call argument IS the goal_original source.
            if let Some(obj) = plan.as_object_mut() {
                obj.insert(
                    "task".into(),
                    serde_json::Value::String(from_call.to_string()),
                );
            }
            from_call.to_string()
        }
        (None, Some(from_call)) => {
            return Err(crate::marshal::schema_violation(
                py,
                format!("task argument must be a non-empty string, got {from_call:?}"),
            ));
        }
        (None, None) => {
            return Err(crate::marshal::schema_violation(
                py,
                "no task: the plan has no task field and no task argument was given (CE-05)"
                    .to_string(),
            ));
        }
    };
    Ok((plan, resolved))
}
