//! hesmos-ffi — the FFI-1 immutable surface (api-contracts FFI-1).
//!
//! This is the ONLY Python→core path (SS-16 rule 1: no bypass exists). P2b completes
//! the nine-function surface: session_open performs the FULL composition (compile →
//! WAL → session.open/plan.compiled events → Running) exactly like the CLI run path,
//! session_run executes the plan through the Python callback registry, and the
//! status/close functions report or retire the core-side state. Surface additions
//! require an api-contracts revision (forbidden: arbitrary Rust exposure, internal
//! structures).
//!
//! Contract amendment applied per backend.md "P1e 후속" consensus (a): plan_hash is a
//! REQUIRED TYPE-5 field, so session_open takes the plan and the task and returns a
//! fully-formed handle — `session_open(seed?, budget?, team_id?, plan, task)`. The
//! Python `Session` therefore defers the native open to its first `run()` so the
//! §6.3 flow (`Session(seed=…, budget=…)` then `core.run(plan, task=…)`) stays intact.
//!
//! D-7: binary crates cannot be depended on, so this crate hosts its own composition
//! adapters (composition.rs) mirroring the CLI's, plus the PY-3 Python bridge.

mod callbacks;
mod composition;
mod marshal;

use std::cell::RefCell;
use std::path::PathBuf;

use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyModule};

use hesmos_core::{
    BathosEngine, BudgetEnvelope, SessionId, SessionState, TeamId, canonical_sha256, code_to_static,
};
use hesmos_orchestrator::{
    DeterministicEngine, GraphEngine, LoopGuardRouter, RunOutcome, Runner, RunnerConfig,
    parse_plan, parse_team_id,
};
use hesmos_trace::seal;

use crate::composition::PyBridgeExecutor;
type CoreHandle = hesmos_core::SessionHandle;

/// The native session handle returned by session_open. `unsendable` because the
/// runner holds non-Send seam trait objects; all access is GIL-serialised, which is
/// exactly pyo3's single-threaded contract for unsendable classes.
#[pyclass(unsendable)]
struct SessionHandle {
    inner: RefCell<Inner>,
}

struct Inner {
    session_id: SessionId,
    seed: u64,
    team_id: Option<TeamId>,
    budget: BudgetEnvelope,
    /// The parsed plan (typed) — recompile source for dry-run schedules.
    plan: hesmos_core::Plan,
    /// The plan dict AS PASSED IN (pre task-merge) — identity basis for session_run.
    plan_raw: serde_json::Value,
    task: String,
    root: PathBuf,
    /// Some until the real run consumes it (dry-run leaves it intact).
    runner: Option<Runner>,
    /// The terminal TYPE-5 snapshot after a real run (plan_hash immutable, state final).
    final_handle: Option<CoreHandle>,
    outcome: Option<RunOutcome>,
    closed: bool,
}

#[pymethods]
impl SessionHandle {
    #[getter]
    fn session_id(&self) -> String {
        self.inner.borrow().session_id.to_string()
    }

    #[getter]
    fn seed(&self) -> u64 {
        self.inner.borrow().seed
    }
}

/// Downcasts a Python-passed handle to the native pyclass. Any other object is an
/// FFI-SCHEMA rejection — the boundary never probes dict shapes it did not mint.
fn borrow_handle<'a>(
    py: Python<'_>,
    handle: &'a Bound<'_, PyAny>,
) -> PyResult<pyo3::PyRef<'a, SessionHandle>> {
    handle
        .cast::<SessionHandle>()
        .map_err(|_| {
            marshal::schema_violation(
                py,
                "handle must be the native SessionHandle returned by session_open".into(),
            )
        })?
        .try_borrow()
        .map_err(|_| {
            marshal::state_violation(py, "handle is already borrowed (reentrant call)".into())
        })
}

fn borrow_handle_mut<'a>(
    py: Python<'_>,
    handle: &'a Bound<'_, PyAny>,
) -> PyResult<pyo3::PyRefMut<'a, SessionHandle>> {
    handle
        .cast::<SessionHandle>()
        .map_err(|_| {
            marshal::schema_violation(
                py,
                "handle must be the native SessionHandle returned by session_open".into(),
            )
        })?
        .try_borrow_mut()
        .map_err(|_| {
            marshal::state_violation(py, "handle is already borrowed (reentrant call)".into())
        })
}

/// Boundary check for the budget spec (trust boundary — FFI-SCHEMA on violation).
/// Spec form is `{tokens: int>=0}` ONLY: `cost_usd` is rejected exactly like the
/// CLI-4 grammar (no unit table exists; silently dropping a user-specified cost
/// would be a swallowed input — composition.rs metering note).
fn validate_budget_spec(py: Python<'_>, value: &serde_json::Value) -> PyResult<()> {
    let Some(obj) = value.as_object() else {
        return Err(marshal::schema_violation(
            py,
            "budget must be an object {tokens}".into(),
        ));
    };
    match obj.get("tokens") {
        Some(serde_json::Value::Number(n)) if n.as_u64().is_some() => {}
        _ => {
            return Err(marshal::schema_violation(
                py,
                "budget.tokens must be a non-negative integer".into(),
            ));
        }
    }
    if let Some(cost) = obj.get("cost_usd") {
        return Err(marshal::schema_violation(
            py,
            format!(
                "budget.cost_usd cannot be honored yet (token-centric metering, ERD §7) — got {cost}"
            ),
        ));
    }
    Ok(())
}

fn budget_envelope_from_spec(spec: Option<&serde_json::Value>) -> BudgetEnvelope {
    // Absent budget = unbounded, but ALWAYS frozen (SS-15 rule 1 — recorded on
    // session.open either way), same as the CLI's missing --budget.
    let Some(spec) = spec else {
        return BudgetEnvelope::default();
    };
    let tokens = spec.get("tokens").and_then(|v| v.as_u64());
    BudgetEnvelope {
        session_max_tokens: tokens,
        ..BudgetEnvelope::default()
    }
}

fn compile_error(py: Python<'_>, e: &hesmos_core::CompileError) -> PyErr {
    marshal::raise_hesmos(
        py,
        "CompileError",
        "Compile",
        e.code(),
        format!("{e:?}"),
        "fix the plan file (US-02 — compile errors leave no session behind)",
    )
}

fn runner_error(py: Python<'_>, e: hesmos_orchestrator::RunnerError) -> PyErr {
    match e {
        // Compile cannot happen here (session_open pre-compiles) but the conversion
        // stays total — a runner-side CE is still the compiler's verdict.
        hesmos_orchestrator::RunnerError::Compile(ce) => compile_error(py, &ce),
        // WAL/IO failures mid-run are evidence-infrastructure failures: surface, never
        // swallow (데이터 유실 금지). FFI-STATE is the honest code — the session state
        // cannot be advanced safely.
        other => marshal::state_violation(py, format!("session infrastructure failure: {other}")),
    }
}

fn new_seed_from_ulid() -> u64 {
    // ponytail: seed entropy borrowed from ULID random bytes; replace with a dedicated
    // rng only if seed quality is ever questioned (seed is recorded, not secret).
    let bytes = ulid::Ulid::generate().to_bytes();
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

/// FFI-1 ① session_open(seed?, budget?, team_id?, plan, task?) -> SessionHandle.
///
/// Performs the full open exactly like the CLI run path: merge task → parse_plan →
/// compile probe (CE failures leave NO artifacts) → EventLog + ledger → Runner::open
/// (WAL, session.open + plan.compiled events, state=Running). Consensus (a): the
/// response carries the full TYPE-5 identity including plan_hash — read it via
/// session_status.
#[pyfunction]
#[pyo3(signature = (seed=None, budget=None, team_id=None, system_prompt=None, *, plan, task=None))]
fn session_open<'py>(
    py: Python<'py>,
    seed: Option<u64>,
    budget: Option<&Bound<'py, PyAny>>,
    team_id: Option<String>,
    system_prompt: Option<String>,
    plan: &Bound<'py, PyAny>,
    task: Option<String>,
) -> PyResult<Bound<'py, SessionHandle>> {
    let budget_value = match budget {
        Some(b) => {
            let v = marshal::py_to_value(b)?;
            validate_budget_spec(py, &v)?;
            Some(v)
        }
        None => None,
    };
    let plan_raw = marshal::py_to_value(plan)?;
    let (merged, task) = composition::merge_task(py, plan_raw.clone(), task.as_deref())?;
    // Value → YAML text → parse_plan: the same textual parse the CLI performs, so the
    // §6.3 dict surface and the CLI file surface get identical compiler verdicts.
    let yaml_text = serde_yaml_ng::to_string(&merged).map_err(|e| {
        marshal::schema_violation(py, format!("plan is not representable as plan YAML: {e}"))
    })?;
    let parsed = parse_plan(&yaml_text).map_err(|e| compile_error(py, &e))?;

    let budget_env = budget_envelope_from_spec(budget_value.as_ref());
    let team = team_id
        .filter(|t| !t.trim().is_empty())
        .map(|t| parse_team_id(&t));
    // Session ids are unique per session/run — uniqueness (not reproducibility) is
    // required here; trace reproduction keys off plan_hash+seed (ADR-0006).
    let session_id = SessionId::generate();
    let seed = seed.unwrap_or_else(new_seed_from_ulid);

    let root = std::env::current_dir()
        .map_err(|e| marshal::state_violation(py, format!("cwd unavailable: {e}")))?;
    // CE-02..09 probe BEFORE any artifact exists (US-04 AC2 parity with the CLI).
    GraphEngine::compile(&DeterministicEngine, &parsed, seed).map_err(|e| compile_error(py, &e))?;

    // PORT-2 model validate at session start (exceptions.md §5: E-MODEL-MIX rejects
    // the session start; the bathos code is exposed VERBATIM — the adapter wraps,
    // never re-implements the check, SS-17 rule 2). An absent/unspawnable engine is
    // tolerated with the same semantics as seal's exit 127: no engine = no verdict,
    // and substituting our own judgment would BE the forbidden re-implementation.
    let bathos = hesmos_orchestrator::BathosCli::new(
        std::env::var("HESMOS_BATHOS").unwrap_or_else(|_| "bathos".into()),
    );
    match bathos.model_validate() {
        Ok(report) if !report.ok => {
            return Err(marshal::raise_hesmos(
                py,
                "HesmosError",
                "Bathos",
                "E-MODEL-MIX",
                format!(
                    "bathos model validate rejected the session: {}",
                    serde_json::to_string(&report.raw.0)
                        .unwrap_or_else(|_| "<unserializable report>".into())
                ),
                "fix model-plan.json — single backend glm-5.3-flash (SS-17 rule 1)",
            ));
        }
        Ok(_) => {}
        Err(_) => {} // engine absent — proceed unverified (documented degraded mode)
    }

    let policy = composition::load_policy(py)?;
    let (_, _, checkpoint_db) = composition::session_paths(&root, &session_id);
    // ONE log, ONE chain: the runner's sink and the executor's tool.call emission
    // share a single Arc'd EventLog (two independent handles would fork the chain).
    let log = composition::SharedLog::new(
        composition::event_log(&root, &session_id)
            .map_err(|e| marshal::state_violation(py, format!("trace log open failed: {e}")))?,
    );
    // US-26: the judge is snapshotted at OPEN — a judge registered later (or a
    // replacement registered for the NEXT eval) never leaks into this session.
    let judge = callbacks::lookup_judge(py, "default").map(|registration| {
        (
            registration,
            composition::JudgeLedger::new(),
        )
    });
    let judge_ledger = judge.as_ref().map(|(_, ledger)| ledger.clone());
    let meter =
        composition::BudgetMeter::open(&session_id, &checkpoint_db, &budget_env, team.as_ref())
            .map_err(|e| marshal::state_violation(py, format!("ledger open failed: {e}")))?;

    // S3 (SS-18): a session WITH a system prompt freezes its hash HERE — the core
    // conductor verifies it every turn (fail-closed) and a breach halts with zero
    // retries (runner special rule, exceptions.md §4). No prompt = the model-neutral
    // unfrozen world: guard::verify_turn(None, _) is Stable, nothing to verify.
    // Blank text is treated as no prompt (an empty stable layer is meaningless).
    let system_prompt = system_prompt.filter(|t| !t.trim().is_empty());
    let prompt_hash = system_prompt.as_ref().map(canonical_sha256);

    let runner = Runner::open(
        RunnerConfig {
            root: root.clone(),
            policy: policy.clone(),
        },
        &parsed,
        session_id,
        seed,
        budget_env.clone(),
        team.clone(),
        None,
        Box::new(PyBridgeExecutor::new(
            &parsed,
            system_prompt,
            log.clone_handle(),
            judge,
        )),
        Box::new(match judge_ledger {
            Some(ledger) => {
                composition::GuardGates::with_judge(hesmos_guard::LeaseRegistry::new(), ledger)
            }
            None => composition::GuardGates::new(),
        }),
        Box::new(meter),
        Box::new(LoopGuardRouter::new(
            &task,
            composition::GuardValidator,
            policy,
        )),
        Box::new(composition::CacheSentinel {
            frozen: prompt_hash.map(hesmos_guard::PromptInvariant::freeze),
        }),
        Box::new(log),
    )
    .map_err(|e| runner_error(py, e))?;

    Ok(Py::new(
        py,
        SessionHandle {
            inner: RefCell::new(Inner {
                session_id,
                seed,
                team_id: team,
                budget: budget_env,
                plan: parsed,
                plan_raw,
                task,
                root,
                runner: Some(runner),
                final_handle: None,
                outcome: None,
                closed: false,
            }),
        },
    )?
    .into_bound(py)
    .clone())
}

/// FFI-1 ② plan_from_yaml(text) -> plan dict | CompileError(CE-01).
///
/// Deliberately PRE-compile (CE-01 schema-parse + minimal shape only): the §6.3
/// contract plan carries NO task field, and a task-requiring compile (CE-05) would
/// reject it here — but the task legally enters later, at run() (merge_task). The
/// FULL compiler verdicts (CE-02..09) surface at session_open, after task injection.
#[pyfunction]
fn plan_from_yaml<'py>(py: Python<'py>, text: &str) -> PyResult<Bound<'py, PyAny>> {
    let value: serde_json::Value = serde_yaml_ng::from_str(text).map_err(|e| {
        marshal::compile_parse_error(py, format!("plan YAML schema parse failed: {e}"))
    })?;
    let Some(obj) = value.as_object() else {
        return Err(marshal::compile_parse_error(
            py,
            "plan must be a YAML mapping".into(),
        ));
    };
    let Some(stages) = obj.get("stages") else {
        return Err(marshal::compile_parse_error(
            py,
            "plan.stages is required".into(),
        ));
    };
    let Some(stage_list) = stages.as_array() else {
        return Err(marshal::compile_parse_error(
            py,
            "plan.stages must be a list".into(),
        ));
    };
    for stage in stage_list {
        let Some(stage_obj) = stage.as_object() else {
            return Err(marshal::compile_parse_error(
                py,
                "every plan.stages entry must be a mapping".into(),
            ));
        };
        if !matches!(stage_obj.get("id"), Some(serde_json::Value::String(_))) {
            return Err(marshal::compile_parse_error(
                py,
                "every stage requires a string id".into(),
            ));
        }
    }
    marshal::value_to_py(py, &value)
}

/// FFI-1 ③ register_provider(model_ref, callback) — PY-3 registration.
///
/// Signature mismatches surface at CALL time as mechanical failures (consensus b),
/// not here: registration only proves callability.
#[pyfunction]
fn register_provider(py: Python<'_>, model_ref: &str, callback: &Bound<'_, PyAny>) -> PyResult<()> {
    callbacks::register_provider(py, model_ref, callback.clone().unbind())
}

/// FFI-1 ④ register_tool(name, callback, external=False) — PY-4 registration.
/// `external` is the tool layer's registration-time declaration that the tool
/// reads EXTERNAL data (WP-P2e): the executor feeds it into core's
/// external_import_verdict (SS-20 rule 1) at dispatch. Additive amendment
/// (default False, existing registrations unchanged) — recorded in ml.md §7.1.
#[pyfunction(signature = (name, callback, external = false))]
fn register_tool(
    py: Python<'_>,
    name: &str,
    callback: &Bound<'_, PyAny>,
    external: bool,
) -> PyResult<()> {
    callbacks::register_tool(py, name, callback.clone().unbind(), external)
}

/// FFI-1 ⑥-b register_judge(name, callback, prompt_hash, temperature,
/// model_version) — US-26 optional quality judge (WP-P3b). The metadata is
/// registration-time: the bridge (hesmos.eval_judge) computes prompt_hash =
/// sha256(prompt text) and enforces the fixed-temperature policy (None
/// forbidden) BEFORE registering, so a recorded judgment always carries its
/// record. Re-registration REPLACES (drift tracking across evals) — sessions
/// snapshot at open. Additive amendment — recorded in ml.md §7.1.
#[pyfunction]
fn register_judge(
    py: Python<'_>,
    name: &str,
    callback: &Bound<'_, PyAny>,
    prompt_hash: &str,
    temperature: f64,
    model_version: &str,
) -> PyResult<()> {
    callbacks::register_judge(
        py,
        name,
        callbacks::JudgeRegistration {
            callback: callback.clone().unbind(),
            prompt_hash: prompt_hash.to_string(),
            temperature,
            model_version: model_version.to_string(),
        },
    )
}

/// FFI-1 ⑥-c clear_judge(name) -> bool — removes a judge registration (US-26
/// test isolation: the registry is process-global and keyed by name, so suites
/// need a deterministic way back to the rules-only state). Running sessions are
/// unaffected (open-time snapshot). Additive amendment — recorded in ml.md §7.1.
#[pyfunction]
fn clear_judge(name: &str) -> bool {
    callbacks::clear_judge(name)
}

/// FFI-1 ⑤ session_run(handle, plan, task?, dry_run) -> RunReceipt dict.
///
/// plan/task must be IDENTICAL to what the session opened with (plan_hash is
/// immutable after open — SS-04 rule 6): a mismatch is FFI-STATE, never a re-open.
/// dry_run=True verifies the wave schedule with zero node executions (receipt.waves);
/// dry_run=False consumes the runner and returns the real receipt, sealing the trace
/// when the session completes (CLI close_session parity).
#[pyfunction]
#[pyo3(signature = (handle, plan, task=None, dry_run=false))]
fn session_run<'py>(
    py: Python<'py>,
    handle: &Bound<'py, PyAny>,
    plan: &Bound<'py, PyAny>,
    task: Option<String>,
    dry_run: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let binding = borrow_handle_mut(py, handle)?;
    let mut inner = binding.inner.borrow_mut();
    if inner.closed {
        return Err(marshal::state_violation(
            py,
            "session is closed — create a new Session (SS-16 rule 1)".into(),
        ));
    }
    if inner.runner.is_none() {
        return Err(marshal::state_violation(
            py,
            "session already ran to a terminal state — replay with `hesmos trace replay` instead"
                .into(),
        ));
    }
    let plan_value = marshal::py_to_value(plan)?;
    if plan_value != inner.plan_raw {
        return Err(marshal::state_violation(
            py,
            "plan differs from the one the session opened with — plan_hash is immutable (SS-04 rule 6)".into(),
        ));
    }
    let (_, task_resolved) = composition::merge_task(py, plan_value, task.as_deref())?;
    if task_resolved != inner.task {
        return Err(marshal::state_violation(
            py,
            format!(
                "task differs from the one the session opened with: {:?} vs {:?}",
                task_resolved, inner.task
            ),
        ));
    }

    if dry_run {
        // Schedule-only verification: compile is pure, so this touches no state —
        // zero node executions, zero events beyond the open pair (deviation from CLI
        // --dry-run documented: the FFI call has an OPEN session, the CLI flag has none).
        let compiled = GraphEngine::compile(&DeterministicEngine, &inner.plan, inner.seed)
            .map_err(|e| compile_error(py, &e))?;
        let waves: Vec<Vec<String>> = compiled
            .waves()
            .iter()
            .map(|w| w.nodes.iter().map(|n| n.as_str().to_string()).collect())
            .collect();
        let state = inner
            .runner
            .as_ref()
            .map(|r| r.handle().state)
            .unwrap_or(SessionState::Running);
        return receipt_dict(
            py,
            &receipt(&inner, state, None, 0, None, true, Some(waves)),
        );
    }

    let runner = inner.runner.take().expect("checked above");
    let mut final_handle = runner.handle().clone();
    let outcome = runner.run().map_err(|e| runner_error(py, e))?;
    final_handle.state = outcome.final_state;
    final_handle.chain_head = outcome.chain_head.clone();

    // Seal ONLY a completed session (ST-1: COMPLETED —trace seal→ 증거 성립); other
    // terminals stay unsealed so fork/resume can continue from them (CLI parity).
    if outcome.final_state == SessionState::Completed {
        seal_completed(py, &inner)?;
    }
    inner.final_handle = Some(final_handle);
    inner.outcome = Some(outcome);

    let outcome = inner.outcome.as_ref().expect("just stored");
    let reason = outcome.reason.map(code_to_static).map(str::to_string);
    let receipt = receipt(
        &inner,
        outcome.final_state,
        reason,
        outcome.commits,
        inner
            .final_handle
            .as_ref()
            .and_then(|h| h.chain_head.as_ref().map(|s| s.as_str().to_string())),
        false,
        None,
    );
    receipt_dict(py, &receipt)
}

/// Seals a completed session. HESMOS_BATHOS selects the audit binary; exit 127 means
/// it never ran — the LOCAL seal stands (it is appended before the audit call) and
/// only the audit join is deferred (CLI close_session parity). The audited head is
/// always the pre-seal chain head (== outcome.chain_head), so the handle needs no
/// update on either success or the 127 path. A hard seal failure is an
/// evidence-integrity problem: surface, never swallow.
fn seal_completed(py: Python<'_>, inner: &Inner) -> PyResult<()> {
    let log = composition::event_log(&inner.root, &inner.session_id)
        .map_err(|e| marshal::state_violation(py, format!("trace log reopen failed: {e}")))?;
    let engine = hesmos_orchestrator::BathosCli::new(
        std::env::var("HESMOS_BATHOS").unwrap_or_else(|_| "bathos".into()),
    )
    .with_cwd(inner.root.clone());
    match seal(&log, Some(&engine)) {
        Ok(_) => Ok(()),
        Err(hesmos_trace::SealError::Audit {
            bathos_exit: 127, ..
        }) => Ok(()),
        Err(e) => Err(marshal::state_violation(
            py,
            format!("trace seal failed: {e}"),
        )),
    }
}

/// Receipt shape (PY-5): session_id is the first-class field, trace_id the §6.3
/// alias — the two values are always identical (contract note).
struct Receipt {
    session_id: String,
    final_state: String,
    reason: Option<String>,
    commits: u64,
    chain_head: Option<String>,
    dry_run: bool,
    waves: Option<Vec<Vec<String>>>,
}

#[allow(clippy::too_many_arguments)]
fn receipt(
    inner: &Inner,
    state: SessionState,
    reason: Option<String>,
    commits: u64,
    chain_head: Option<String>,
    dry_run: bool,
    waves: Option<Vec<Vec<String>>>,
) -> Receipt {
    Receipt {
        session_id: inner.session_id.to_string(),
        final_state: serde_json::to_value(state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| format!("{state:?}")),
        reason,
        commits,
        chain_head,
        dry_run,
        waves,
    }
}

fn receipt_dict<'py>(py: Python<'py>, r: &Receipt) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("session_id", &r.session_id)?;
    dict.set_item("trace_id", &r.session_id)?;
    dict.set_item("final_state", &r.final_state)?;
    dict.set_item("reason", r.reason.clone())?;
    dict.set_item("commits", r.commits)?;
    dict.set_item("chain_head", r.chain_head.clone())?;
    dict.set_item("dry_run", r.dry_run)?;
    match &r.waves {
        Some(waves) => dict.set_item("waves", waves)?,
        None => dict.set_item("waves", py.None())?,
    }
    Ok(dict)
}

/// FFI-1 ⑥ session_status(handle) -> full TYPE-5 SessionHandle snapshot.
///
/// Consensus (a): plan_hash is ALWAYS present (it exists from plan.compiled on).
/// Post-run the terminal snapshot answers; pre-run the live runner handle does.
#[pyfunction]
fn session_status<'py>(
    py: Python<'py>,
    handle: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    let binding = borrow_handle(py, handle)?;
    let inner = binding.inner.borrow();
    if inner.closed {
        return Err(marshal::state_violation(py, "session is closed".into()));
    }
    let core_handle = match &inner.final_handle {
        Some(h) => h,
        None => inner
            .runner
            .as_ref()
            .map(|r| r.handle())
            .ok_or_else(|| marshal::state_violation(py, "session has no live handle".into()))?,
    };
    let value = serde_json::to_value(core_handle)
        .map_err(|e| marshal::schema_violation(py, format!("handle serialization failed: {e}")))?;
    marshal::value_to_py(py, &value)?
        .cast_into::<PyDict>()
        .map_err(|e| marshal::schema_violation(py, format!("handle snapshot not a dict: {e}")))
}

/// FFI-1 ⑦ budget_status(handle) -> BudgetState snapshot (TYPE-agnostic core shape).
///
/// Reads the session ledger's own totals — after a real run this is exactly the
/// reconciled AC3 number (ledger sums). The neutral agent role means "no role
/// selected": agent-scope lines report uncapped (composition.rs note).
#[pyfunction]
fn budget_status<'py>(py: Python<'py>, handle: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    let binding = borrow_handle(py, handle)?;
    let inner = binding.inner.borrow();
    if inner.closed {
        return Err(marshal::state_violation(py, "session is closed".into()));
    }
    let (_, _, checkpoint_db) = composition::session_paths(&inner.root, &inner.session_id);
    let state = composition::budget_state_snapshot(
        &inner.session_id,
        &checkpoint_db,
        &inner.budget,
        inner.team_id.as_ref(),
    )
    .map_err(|e| marshal::state_violation(py, format!("ledger read failed: {e}")))?;
    let value = serde_json::to_value(state).map_err(|e| {
        marshal::schema_violation(py, format!("budget state serialization failed: {e}"))
    })?;
    marshal::value_to_py(py, &value)?
        .cast_into::<PyDict>()
        .map_err(|e| marshal::schema_violation(py, format!("budget snapshot not a dict: {e}")))
}

/// FFI-1 ⑧ build_messages(handle, stage, snapshot) -> list[Message dicts].
///
/// ponytail: marshaling SKELETON only — one user Message carrying task + snapshot.
/// The 3-layer breakpoint builder (stable/context/volatile placement, SS-18 rule 5)
/// is WP-P2d and swaps this interior wholesale; the signature and the returned
/// Message shape are already the contract surface.
#[pyfunction]
fn build_messages<'py>(
    py: Python<'py>,
    handle: &Bound<'py, PyAny>,
    stage: &Bound<'py, PyAny>,
    snapshot: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let binding = borrow_handle(py, handle)?;
    let inner = binding.inner.borrow();
    if inner.closed {
        return Err(marshal::state_violation(py, "session is closed".into()));
    }
    let stage_value = marshal::py_to_value(stage)?;
    let node = stage_value
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            marshal::schema_violation(py, "stage must carry a string id (StageSpec shape)".into())
        })?;
    let snapshot_value = marshal::py_to_value(snapshot)?;
    let snapshot_text = serde_json::to_string(&snapshot_value)
        .map_err(|e| marshal::schema_violation(py, format!("snapshot not serializable: {e}")))?;
    let message = PyDict::new(py);
    message.set_item("role", "user")?;
    message.set_item("content", format!("{}\n\n{}", inner.task, snapshot_text))?;
    message.set_item("tool_call_id", py.None())?;
    let _ = node; // node selection enters with the P2d builder (layer assignment per stage)
    let list = pyo3::types::PyList::empty(py);
    list.append(message)?;
    Ok(list.into_any())
}

/// FFI-1 ⑨ session_close(handle) -> None. Idempotent; frees the live runner (a
/// never-ran session stays Running in its WAL — the same state a suspended CLI
/// session leaves, resumable via trace replay).
#[pyfunction]
fn session_close(py: Python<'_>, handle: &Bound<'_, PyAny>) -> PyResult<()> {
    let binding = borrow_handle_mut(py, handle)?;
    let mut inner = binding.inner.borrow_mut();
    inner.closed = true;
    inner.runner.take(); // drop the runner; artifacts remain on disk
    Ok(())
}

#[pymodule]
fn _ffi(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<SessionHandle>()?;
    module.add_function(wrap_pyfunction!(session_open, module)?)?;
    module.add_function(wrap_pyfunction!(plan_from_yaml, module)?)?;
    module.add_function(wrap_pyfunction!(register_provider, module)?)?;
    module.add_function(wrap_pyfunction!(register_tool, module)?)?;
    module.add_function(wrap_pyfunction!(register_judge, module)?)?;
    module.add_function(wrap_pyfunction!(clear_judge, module)?)?;
    module.add_function(wrap_pyfunction!(session_run, module)?)?;
    module.add_function(wrap_pyfunction!(session_status, module)?)?;
    module.add_function(wrap_pyfunction!(budget_status, module)?)?;
    module.add_function(wrap_pyfunction!(build_messages, module)?)?;
    module.add_function(wrap_pyfunction!(session_close, module)?)?;
    Ok(())
}
