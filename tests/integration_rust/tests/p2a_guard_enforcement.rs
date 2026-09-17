//! WP-P2a E2E — the SS-19 / SS-20 / SS-18-core enforcement through the REAL
//! composition (T12 코어부): the same guard gates, cache sentinel and runner the
//! binary dispatches through.
//!
//! ① AC3: a handoff contract granting a tool the GRANTOR lacks is rejected at the
//!   receiver's Pre boundary — gate.fail(permission), session FAILED, exit 20; the
//!   compliant twin completes (권한 상승 불가, no union across the chain).
//! ② AC1: a profile-less stage never reaches execution — CE-06 at compile (exit 3),
//!   with no session artifacts left behind.
//! ③ AC5 / S3 core half: a prompt-hash violation on a frozen session halts
//!   IMMEDIATELY — violation event (gate_id=cache), FAILED(GATE_REJECT), exit 20,
//!   and ZERO retries even with a non-empty bounded-retry budget (exceptions.md §4
//!   special row: a deterministic invariant breach is not retryable).
//!
//! The cache scenario drives `Runner::open` directly (the CLI has no frozen-prompt
//! surface in W5 — the real prompt builder arrives with WP-P2d), but composes the
//! production adapters: GuardGates, BudgetMeter, LoopGuardRouter, CacheSentinel.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use hesmos::cmd;
use hesmos::composition::{BudgetMeter, CacheSentinel, GuardGates, GuardValidator, reason_exit};
use hesmos::exit;
use hesmos_core::{EventKind, SessionId};
use hesmos_guard::PromptInvariant;
use hesmos_orchestrator::{
    ExecutionReport, LoopGuardRouter, Runner, RunnerConfig, StageExecutor, StageFailure,
    StageRequest,
};
use hesmos_trace::load_path;

// ---------------------------------------------------------------------------
// Harness (same shape as p1e_cli_e2e — fresh root per test, no env needed)
// ---------------------------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("hesmos-p2a-{name}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        Self(dir)
    }

    fn write_plan(&self, text: &str) -> PathBuf {
        let path = self.0.join("plan.hes");
        std::fs::write(&path, text).expect("write plan");
        path
    }

    fn the_session(&self) -> SessionId {
        let sessions = self.0.join(".hesmos").join("sessions");
        let mut entries: Vec<_> = std::fs::read_dir(&sessions)
            .expect("sessions dir exists")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one session in a fresh root");
        let id = entries.pop().expect("len 1");
        hesmos_orchestrator::parse_session_id(&id).expect("session id parses")
    }

    fn events(&self, id: &SessionId) -> Vec<hesmos_core::TraceEvent> {
        let path = self
            .0
            .join(".hesmos")
            .join("sessions")
            .join(id.to_string())
            .join("events.jsonl");
        match load_path(&path) {
            hesmos_trace::LoadOutcome::Ok(events) => events,
            other => panic!("chain must be intact: {other:?}"),
        }
    }

    fn sessions_root(&self) -> PathBuf {
        self.0.join(".hesmos").join("sessions")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_args(plan: &Path, seed: u64) -> cmd::run::RunArgs {
    cmd::run::RunArgs {
        plan: plan.to_path_buf(),
        seed: Some(seed),
        budget: Some("tokens=100000".into()),
        team: None,
        dry_run: false,
    }
}

// ---------------------------------------------------------------------------
// ① AC3 — the permission chain rule, both directions
// ---------------------------------------------------------------------------

/// Two-node chain; `downstream_tools` is the receiver's allowlist, which the handoff
/// contract from `fetch` grants. When the grant exceeds what fetch holds, the
/// receiver's Pre permission gate must reject the session.
fn two_node_plan(downstream_tools: &str) -> String {
    format!(
        r#"
name: p2a-permission
task: fetch and summarize
pattern: graph
stages:
  - id: fetch
    profile: {{role: researcher, model: glm-5.3-flash, tools: [web.search]}}
    input_schema: fetch.v1
    done_criteria: {{items: [notes]}}
  - id: summarize
    profile: {{role: researcher, model: glm-5.3-flash, tools: {downstream_tools}}}
    input_schema: summary.v1
    done_criteria: {{items: [summary]}}
    depends: [fetch]
"#
    )
}

/// US-23 AC3 / SS-19 rule 2: `fetch` (grantor: web.search) hands work whose
/// permission_cap grants `write.file` — privilege amplification. The gate.fail lands
/// at the RECEIVER's Pre boundary; the session fails with exit 20 and the violating
/// node never executes.
#[test]
fn t12_cap_beyond_grantor_rejects_at_receiver_exit20() {
    let root = TempRoot::new("cap-violation");
    let plan = root.write_plan(&two_node_plan("[write.file]"));

    let code = cmd::run::execute(run_args(&plan, 42), &root.0);
    assert_eq!(code, exit::EXIT_FAILED, "권한 상승 계약은 exit 20");

    let session = root.the_session();
    let events = root.events(&session);

    // The judgment event: gate.fail(permission, GATE_REJECT) at summarize.
    let fails: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::GateFail)
        .collect();
    assert!(
        fails
            .iter()
            .any(|e| e.attrs.get_str("gate_id") == Some("permission")
                && e.attrs.get_str("reason_code") == Some("GATE_REJECT")
                && e.node.as_ref().map(|n| n.as_str()) == Some("summarize")),
        "permission gate.fail recorded at the receiver: {fails:?}"
    );

    // Immediate: only fetch EXECUTED — node.start fires at boundary opening (before
    // the pre-gates), so the proof the violating node never ran is the llm.call count.
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::LlmCall)
            .count(),
        1,
        "only the grantor executes — the violating node never runs"
    );
    assert!(
        events.iter().any(|e| e.kind == EventKind::SessionClose
            && e.attrs.get_str("final_state") == Some("FAILED")),
        "session closes FAILED"
    );
    let wal = hesmos_orchestrator::SessionWal::open(
        &root
            .sessions_root()
            .join(session.to_string())
            .join("checkpoint.db"),
    )
    .expect("wal opens");
    assert_eq!(
        wal.commits(&session).expect("commits").len(),
        1,
        "only the grantor's commit exists — the violation stopped the chain"
    );
}

/// The compliant twin: the cap grants only what the grantor holds — the same chain
/// completes. (Subset direction proven so test ①'s failure is the RULE, not the
/// plan shape.)
#[test]
fn t12_cap_within_grantor_completes() {
    let root = TempRoot::new("cap-compliant");
    // Receiver holds NOTHING extra: its cap (its own tools) is the empty set ⊆ grantor.
    let plan = root.write_plan(&two_node_plan("[]"));

    let code = cmd::run::execute(run_args(&plan, 42), &root.0);
    assert_eq!(code, exit::EXIT_OK, "compliant chain completes");

    let session = root.the_session();
    let events = root.events(&session);
    assert!(
        !events.iter().any(|e| e.kind == EventKind::GateFail),
        "no gate failed on the compliant chain"
    );
    assert!(
        events.iter().any(|e| e.kind == EventKind::SessionClose
            && e.attrs.get_str("final_state") == Some("COMPLETED")),
        "session completes"
    );
}

// ---------------------------------------------------------------------------
// ② AC1 — profile-less stage: CE-06 at compile, no session left behind
// ---------------------------------------------------------------------------

#[test]
fn t12_profileless_stage_is_ce06_with_no_session_artifacts() {
    let root = TempRoot::new("profileless");
    let plan = root.write_plan(
        r#"
name: p2a-ce06
task: nobody defined who does this
pattern: graph
stages:
  - id: mystery
    input_schema: m.v1
    done_criteria: {items: [anything]}
"#,
    );

    let code = cmd::run::execute(run_args(&plan, 42), &root.0);
    assert_eq!(
        code,
        exit::EXIT_COMPILE,
        "profile-less stage is CE-06 (exit 3)"
    );
    assert!(
        !root.sessions_root().exists(),
        "compile failure leaves no session artifacts (no-CE-artifact guarantee)"
    );
}

// ---------------------------------------------------------------------------
// ③ AC5 / S3 core half — the cache violation halt, zero retries, exit 20
// ---------------------------------------------------------------------------

/// An executor that reports the WRONG prompt hash every turn — the drift the SS-18
/// sentinel must catch on the first turn. The call counter rides in an Arc so it
/// survives the runner consuming the executor.
struct PromptDriftExecutor {
    calls: Rc<Cell<usize>>,
}

impl StageExecutor for PromptDriftExecutor {
    fn execute(&self, req: &StageRequest) -> Result<ExecutionReport, StageFailure> {
        self.calls.set(self.calls.get() + 1);
        // A "tampered" system prompt: a different byte-stable hash than the frozen one.
        let drifted = hesmos_core::canonical_sha256(&String::from("tampered system prompt"));
        let _ = req;
        Ok(ExecutionReport {
            payload: serde_json::json!({ "node": req.node.as_str() }),
            confidence: 0.95,
            tokens_in: 10,
            tokens_out: 10,
            latency_ms: 0,
            provider: "echo".into(),
            prompt_hash: Some(drifted),
            external_origin: None,
        })
    }
}

#[test]
fn t12_cache_violation_halts_immediately_zero_retries_exit20() {
    let root = TempRoot::new("cache-halt");
    let plan_text = r#"
name: p2a-cache
task: hold the prompt stable
pattern: graph
stages:
  - id: first
    profile: {role: researcher, model: glm-5.3-flash}
    input_schema: a.v1
    done_criteria: {items: [a]}
  - id: second
    profile: {role: researcher, model: glm-5.3-flash}
    input_schema: b.v1
    done_criteria: {items: [b]}
    depends: [first]
"#;
    let plan = hesmos_orchestrator::parse_plan(plan_text).expect("plan parses");

    let session_id = SessionId::generate();
    let (session_dir, _events_path, checkpoint_db) = {
        let dir = root
            .0
            .join(".hesmos")
            .join("sessions")
            .join(session_id.to_string());
        (
            dir.clone(),
            dir.join("events.jsonl"),
            dir.join("checkpoint.db"),
        )
    };
    std::fs::create_dir_all(&session_dir).expect("session dir");

    let log = hesmos_trace::EventLog::open(&root.0, &session_id).expect("event log");
    let meter =
        BudgetMeter::open(&session_id, &checkpoint_db, &Default::default(), None).expect("ledger");

    // The frozen invariant: the session DECLARED this system prompt hash.
    let frozen = hesmos_core::canonical_sha256(&String::from("the one true system prompt"));
    let policy = hesmos_guard::parse_str("bounded_retry: 3").expect("policy");

    let executor_calls = Rc::new(Cell::new(0usize));
    let style = hesmos::tokens::Style { color: false };
    let runner = Runner::open(
        RunnerConfig {
            root: root.0.clone(),
            policy: policy.clone(),
        },
        &plan,
        session_id,
        42,
        Default::default(),
        None,
        None,
        Box::new(PromptDriftExecutor {
            calls: Rc::clone(&executor_calls),
        }),
        Box::new(GuardGates::new()),
        Box::new(meter),
        Box::new(LoopGuardRouter::new(&plan.task, GuardValidator, policy)),
        // The production adapter over the REAL guard verifier — frozen, so every
        // turn owes a matching hash report.
        Box::new(CacheSentinel {
            frozen: Some(PromptInvariant::freeze(frozen)),
        }),
        Box::new(hesmos::composition::ProgressTee { log, style }),
    )
    .expect("open");

    let run_id = runner.handle().run_id;
    let outcome = runner.run().expect("run");

    // Immediate halt: FAILED(GATE_REJECT) — the GATE_REJECT exit band is 20, and the
    // W3-부5 registry settled the cache violation into it (never CANCELLED/resumable).
    assert_eq!(outcome.final_state, hesmos_core::SessionState::Failed);
    assert_eq!(
        outcome.reason,
        Some(hesmos_core::ReasonCode::GATE_REJECT),
        "run_id {run_id}: the violation is a GATE_REJECT family failure"
    );
    assert_eq!(
        reason_exit(outcome.reason, false),
        exit::EXIT_FAILED,
        "cache violation maps to exit 20 (project-context §7 W3-부5)"
    );

    // 재시도 0건 (exceptions §4): ONE call total — the bounded budget (3) is untouched.
    assert_eq!(
        executor_calls.get(),
        1,
        "the special no-retry rule: a deterministic breach is never retried"
    );

    // The violation event on the real chain + the void turn (no llm.call).
    let events = root.events(&session_id);
    let fails: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::GateFail)
        .collect();
    assert_eq!(fails.len(), 1, "exactly the violation, nothing retried");
    assert_eq!(fails[0].attrs.get_str("gate_id"), Some("cache"));
    assert_eq!(fails[0].attrs.get_str("reason_code"), Some("GATE_REJECT"));
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::LlmCall)
            .count(),
        0,
        "the violating turn is void — no llm.call event"
    );
    assert!(
        events.iter().any(|e| e.kind == EventKind::SessionClose
            && e.attrs.get_str("final_state") == Some("FAILED")),
        "session closes FAILED on the record"
    );
}
