//! WP-P3a E2E — the golden eval harness (CLI-5), the replay UX maturation, and the
//! W3-7 failure-reproduction proofs (T10 core + S8 demo loop, CLI level).
//!
//! Scenarios: eval before bless → 3 (bless hint) · `--bless` writes the golden and
//! is a byte-identical no-op on re-approval · eval after bless → 0 · a flipped
//! executor flips gate verdicts → structural regression exit 20 · `--json` report
//! shape (via `execute_captured`) · missing suite → 3 · replay regenerates its own
//! stale fork (CLI-3 idempotency) · BUDGET_EXCEEDED and GATE_REJECT reproduce with
//! the SAME band, SAME structure and SAME reason through the full CLI path.
//!
//! Environment discipline: identical to p1e — env is process-global, every
//! env-sensitive scenario holds [`ENV_LOCK`] and resets the keys it reads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hesmos::cmd;
use hesmos_core::{CommitSeq, EventKind, SessionId};
use hesmos_orchestrator::{SessionWal, derive_fork_session_id};
use hesmos_trace::{compare_structure, project_structure};

// ---------------------------------------------------------------------------
// Harness (same shape as p1e_cli_e2e)
// ---------------------------------------------------------------------------

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("hesmos-p3a-{name}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        Self(dir)
    }

    fn write_plan(&self, text: &str) -> PathBuf {
        let path = self.0.join("plan.hes");
        std::fs::write(&path, text).expect("write plan");
        path
    }

    /// The single session dir under this root (fresh roots keep it unambiguous).
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
        match hesmos_trace::load_path(&path) {
            hesmos_trace::LoadOutcome::Ok(events) => events,
            other => panic!("chain must be intact: {other:?}"),
        }
    }

    fn checkpoint_db(&self, id: &SessionId) -> PathBuf {
        self.0
            .join(".hesmos")
            .join("sessions")
            .join(id.to_string())
            .join("checkpoint.db")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

fn clear_env(key: &str) {
    // SAFETY: caller holds env_lock (see p1e harness discipline).
    unsafe { std::env::remove_var(key) };
}

fn set_env(key: &str, value: &str) {
    // SAFETY: caller holds env_lock.
    unsafe { std::env::set_var(key, value) };
}

fn clean_env() {
    clear_env("HESMOS_EXECUTOR");
    clear_env("HESMOS_POLICY_YAML");
    clear_env("HESMOS_BATHOS");
    clear_env("HESMOS_OTEL_ENDPOINT");
}

/// The 4-node fan-out plan (same shape as the p1e scenarios — two waves, 4
/// boundaries, every gate on the happy path a pass).
const PLAN: &str = r#"
name: p3a-e2e
task: produce the weekly report
pattern: graph
stages:
  - id: fetch
    profile: {role: researcher, model: glm-5.3-flash, tools: [web.search]}
    input_schema: fetch.v1
    done_criteria: {items: [notes]}
  - id: summarize
    profile: {role: researcher, model: glm-5.3-flash}
    input_schema: summary.v1
    done_criteria: {items: [summary]}
    depends: [fetch]
  - id: verify
    profile: {role: reviewer, model: glm-5.3-flash}
    input_schema: check.v1
    done_criteria: {items: [verdict]}
    depends: [fetch]
  - id: done
    profile: {role: writer, model: glm-5.3-flash}
    input_schema: done.v1
    done_criteria: {items: [done]}
    depends: [summarize, verify]
"#;

fn run_args(plan: &Path, seed: u64, budget: &str) -> cmd::run::RunArgs {
    cmd::run::RunArgs {
        plan: plan.to_path_buf(),
        seed: Some(seed),
        budget: Some(budget.into()),
        team: Some("p3a-team".into()),
        dry_run: false,
    }
}

fn eval_args(suite: PathBuf, bless: Option<String>, json: bool) -> cmd::eval::EvalArgs {
    cmd::eval::EvalArgs { suite, bless, json }
}

/// A one-case suite bound to `session`, written inside the root.
fn write_suite(root: &TempRoot, case_id: &str, session: &SessionId) -> PathBuf {
    let path = root.0.join("suite.yaml");
    std::fs::write(
        &path,
        format!("name: p3a-suite\ncases:\n  - id: {case_id}\n    session: {session}\n"),
    )
    .expect("write suite");
    path
}

fn golden_path(suite: &Path) -> PathBuf {
    suite.with_file_name("suite.golden.yaml")
}

/// Final_state string from a chain's session.close.
fn close_state(events: &[hesmos_core::TraceEvent]) -> String {
    events
        .iter()
        .find(|e| e.kind == EventKind::SessionClose)
        .expect("session.close present")
        .attrs
        .get_str("final_state")
        .expect("final_state")
        .to_string()
}

/// The last gate.fail's reason_code on a chain (the W3-7 reproduction fingerprint
/// for gate/loop terminations).
fn last_gate_fail_reason(events: &[hesmos_core::TraceEvent]) -> Option<String> {
    events
        .iter()
        .rfind(|e| e.kind == EventKind::GateFail)
        .and_then(|e| e.attrs.get_str("reason_code").map(String::from))
}

// ---------------------------------------------------------------------------
// T10 — the eval harness loop
// ---------------------------------------------------------------------------

/// The full S8 loop at CLI level: bless → 0 + golden written · re-bless is a
/// byte-identical no-op · eval → 0 · a flipped executor flips gate verdicts →
/// exit 20 · eval before bless → 3. Also proves eval's own idempotency: the
/// second eval regenerates the deterministic fork instead of refusing it.
#[test]
fn t10_bless_eval_regression_loop() {
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("eval-loop");
    let plan = root.write_plan(PLAN);
    assert_eq!(
        cmd::run::execute(run_args(&plan, 42, "tokens=100000"), &root.0),
        0
    );
    let session = root.the_session();
    let suite = write_suite(&root, "happy", &session);

    // Unapproved suite → 3 with the bless hint (never a self-approval).
    assert_eq!(
        cmd::eval::execute(eval_args(suite.clone(), None, false), &root.0),
        3,
        "eval before bless exits 3"
    );
    assert!(
        !golden_path(&suite).exists(),
        "eval never writes the golden"
    );

    // --bless runs the case and records the golden → 0.
    assert_eq!(
        cmd::eval::execute(
            eval_args(suite.clone(), Some(session.to_string()), false),
            &root.0
        ),
        0,
        "bless exits 0"
    );
    assert!(golden_path(&suite).exists(), "bless wrote the golden file");
    let golden_bytes = std::fs::read(golden_path(&suite)).expect("golden readable");

    // Re-bless the SAME sample: idempotent — exit 0, file untouched.
    assert_eq!(
        cmd::eval::execute(
            eval_args(suite.clone(), Some(session.to_string()), false),
            &root.0
        ),
        0,
        "re-bless exits 0"
    );
    assert_eq!(
        std::fs::read(golden_path(&suite)).expect("golden reread"),
        golden_bytes,
        "idempotent bless rewrites nothing (byte-identical no-op)"
    );

    // Eval with the blessed golden: the fork reproduces the structure → 0.
    assert_eq!(
        cmd::eval::execute(eval_args(suite.clone(), None, false), &root.0),
        0,
        "eval after bless exits 0"
    );

    // Structural regression: the reject executor + a raised confidence floor
    // exhaust the route retries at the FIRST handoff — same plan, different
    // STRUCTURE — the S8 signal (the p1e band-20 recipe).
    set_env("HESMOS_EXECUTOR", "reject");
    set_env("HESMOS_POLICY_YAML", "min_confidence: 0.5");
    assert_eq!(
        cmd::eval::execute(eval_args(suite.clone(), None, false), &root.0),
        20,
        "regressed structure exits 20"
    );
    clear_env("HESMOS_EXECUTOR");
    clear_env("HESMOS_POLICY_YAML");

    // A second eval run in the clean env must ALSO pass: eval's fork regeneration
    // cleared the previous (regressed) fork and reproduced the blessed structure.
    assert_eq!(
        cmd::eval::execute(eval_args(suite, None, false), &root.0),
        0,
        "eval is idempotent — the deterministic fork is regenerated"
    );
}

/// `--json` report shape (machine-readable CLI-5): one JSON object with suite,
/// per-case status/mismatches and the exit band. Driven via `execute_captured`
/// (in-process stdout capture is not available).
#[test]
fn t10_json_report_shape_pass_and_regression() {
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("eval-json");
    let plan = root.write_plan(PLAN);
    assert_eq!(
        cmd::run::execute(run_args(&plan, 42, "tokens=100000"), &root.0),
        0
    );
    let session = root.the_session();
    let suite = write_suite(&root, "happy", &session);

    // Unapproved: the json report still carries the verdict (status error + hint).
    let (code, report) = cmd::eval::execute_captured(eval_args(suite.clone(), None, true), &root.0);
    assert_eq!(code, 3, "unapproved suite reports via json too");
    let v: serde_json::Value =
        serde_json::from_str(report.as_deref().expect("json report")).expect("parses");
    assert_eq!(v["suite"], "p3a-suite");
    assert_eq!(v["exit"], 3);
    assert_eq!(v["results"][0]["status"], "error");
    assert!(
        v["results"][0]["message"]
            .as_str()
            .expect("bless hint present")
            .contains("--bless"),
        "the json error carries the bless hint: {}",
        v["results"][0]["message"]
    );

    // Bless, then the json PASS shape.
    let (code, _bless_report) = cmd::eval::execute_captured(
        eval_args(suite.clone(), Some(session.to_string()), false),
        &root.0,
    );
    assert_eq!(code, 0);
    let (code, report) = cmd::eval::execute_captured(eval_args(suite.clone(), None, true), &root.0);
    assert_eq!(code, 0);
    let v: serde_json::Value =
        serde_json::from_str(report.as_deref().expect("json report")).expect("parses");
    assert_eq!(v["exit"], 0);
    assert_eq!(v["results"][0]["status"], "pass");
    assert_eq!(v["results"][0]["id"], "happy");
    assert_eq!(v["results"][0]["session"], session.to_string());

    // Regression shape: mismatch status + at least one rendered diff item.
    set_env("HESMOS_EXECUTOR", "reject");
    set_env("HESMOS_POLICY_YAML", "min_confidence: 0.5");
    let (code, report) = cmd::eval::execute_captured(eval_args(suite, None, true), &root.0);
    clear_env("HESMOS_EXECUTOR");
    clear_env("HESMOS_POLICY_YAML");
    assert_eq!(code, 20);
    let v: serde_json::Value =
        serde_json::from_str(report.as_deref().expect("json report")).expect("parses");
    assert_eq!(v["exit"], 20);
    assert_eq!(v["results"][0]["status"], "mismatch");
    assert!(
        !v["results"][0]["mismatches"]
            .as_array()
            .expect("diff items")
            .is_empty(),
        "the regression report itemizes the structural diff"
    );
}

/// Config-band edges: a missing suite → 3 (CLI-5 has no usage band); a suite whose
/// case id duplicates a golden key → 3 before anything executes.
#[test]
fn t10_suite_config_edges_exit_3() {
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("eval-config");
    let plan = root.write_plan(PLAN);
    assert_eq!(
        cmd::run::execute(run_args(&plan, 42, "tokens=100000"), &root.0),
        0
    );

    // Missing suite (path form and bare-name form both miss) → 3.
    assert_eq!(
        cmd::eval::execute(
            eval_args(root.0.join("no-such-suite.yaml"), None, false),
            &root.0
        ),
        3
    );
    assert_eq!(
        cmd::eval::execute(
            eval_args(PathBuf::from("no-such-suite"), None, false),
            &root.0
        ),
        3
    );

    // A suite naming an unknown session → 3 (evidence/config), not a panic.
    let bogus = SessionId::generate();
    let suite = write_suite(&root, "happy", &bogus);
    assert_eq!(
        cmd::eval::execute(eval_args(suite, None, false), &root.0),
        3,
        "unknown origin session is a config failure"
    );
}

// ---------------------------------------------------------------------------
// W3-7 — failure reproduction through the full CLI path
// ---------------------------------------------------------------------------

/// BUDGET_EXCEEDED: a tiny budget suspends the origin (exit 10); replaying from the
/// last commit with the SAME tiny budget re-pays the cached prefix, hits the same
/// suspend line, and reproduces the same band, structure and terminal facts.
#[test]
fn w3_budget_exceeded_reproduces_same_band_and_structure() {
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("repro-budget");
    let plan = root.write_plan(PLAN);
    assert_eq!(
        cmd::run::execute(run_args(&plan, 7, "tokens=100"), &root.0),
        10,
        "tiny budget suspends the origin"
    );
    let origin = root.the_session();
    let origin_events = root.events(&origin);
    assert_eq!(close_state(&origin_events), "SUSPENDED");

    // The last commit point (the suspend guidance's resume point).
    let wal = SessionWal::open(&root.checkpoint_db(&origin)).expect("wal");
    let commits = wal.commits(&origin).expect("commits");
    assert!(!commits.is_empty(), "suspended after at least one commit");
    let last = commits.last().expect("last commit").seq;

    // Replay from that point, SAME budget: the fork re-pays the prefix from the
    // cache and suspends again — same band.
    let code = cmd::trace::replay(
        cmd::trace::ReplayArgs {
            session_id: origin.to_string(),
            at: Some(last.0),
            budget: Some("tokens=100".into()),
        },
        &root.0,
    );
    assert_eq!(code, 10, "the reproduction suspends with the same band");

    // Artifacts: same structure, same terminal state, and the budget-suspend
    // evidence (suspend-line BudgetEvent) on BOTH chains.
    let fork = derive_fork_session_id(&origin, last);
    let fork_events = root.events(&fork);
    assert_eq!(
        project_structure(&origin_events),
        project_structure(&fork_events),
        "the reproduction replays the same path and verdicts"
    );
    assert_eq!(close_state(&fork_events), "SUSPENDED");
    let has_suspend_event = |events: &[hesmos_core::TraceEvent]| {
        events.iter().any(|e| {
            e.kind == EventKind::BudgetEvent && e.attrs.get_str("level") == Some("suspend")
        })
    };
    assert!(has_suspend_event(&origin_events), "origin suspend recorded");
    assert!(has_suspend_event(&fork_events), "fork suspend recorded");
}

/// GATE_REJECT: the reject executor fails the origin (exit 20); a whole-session
/// replay reproduces the failure — same band, same structure, and the SAME reason
/// code on the terminal gate.fail (the reject path, NOT the cache-violation path:
/// gate_id stays the real gate).
#[test]
fn w3_gate_reject_reproduces_same_band_reason_and_structure() {
    let _env = env_lock();
    clean_env();
    // The p1e band-20 recipe: reject executor lowers contract confidence; the
    // raised floor exhausts the bounded route retries → FAILED(GATE_REJECT).
    set_env("HESMOS_EXECUTOR", "reject");
    set_env("HESMOS_POLICY_YAML", "min_confidence: 0.5");
    let root = TempRoot::new("repro-gate-reject");
    let plan = root.write_plan(PLAN);
    assert_eq!(
        cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0),
        20,
        "the reject executor fails the origin with GATE_REJECT"
    );
    let origin = root.the_session();
    let origin_events = root.events(&origin);
    assert_eq!(close_state(&origin_events), "FAILED");
    assert_eq!(
        last_gate_fail_reason(&origin_events).as_deref(),
        Some("GATE_REJECT")
    );
    // Thomas N1-④: the reject path is a REAL gate, never the cache-violation face.
    assert!(
        origin_events
            .iter()
            .filter(|e| e.kind == EventKind::GateFail)
            .all(|e| e.attrs.get_str("gate_id") != Some("cache")),
        "gate rejects carry their real gate_id"
    );

    // Whole-session replay (no commits needed): same failure again.
    let code = cmd::trace::replay(
        cmd::trace::ReplayArgs {
            session_id: origin.to_string(),
            at: None,
            budget: None,
        },
        &root.0,
    );
    assert_eq!(code, 20, "the reproduction fails with the same band");

    let fork = derive_fork_session_id(&origin, CommitSeq(0));
    let fork_events = root.events(&fork);
    assert_eq!(
        project_structure(&origin_events),
        project_structure(&fork_events),
        "the reproduction replays the same path and verdicts"
    );
    assert_eq!(close_state(&fork_events), "FAILED");
    assert_eq!(
        last_gate_fail_reason(&fork_events).as_deref(),
        Some("GATE_REJECT"),
        "the same reason code reproduces"
    );
    clear_env("HESMOS_EXECUTOR");
    clear_env("HESMOS_POLICY_YAML");
}

/// CLI-3 idempotency: replaying the SAME point twice regenerates the stale fork
/// (same lineage) instead of refusing with USAGE-STATE — and both runs agree.
#[test]
fn replay_is_idempotent_via_lineage_verified_regeneration() {
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("replay-idempotent");
    let plan = root.write_plan(PLAN);
    assert_eq!(
        cmd::run::execute(run_args(&plan, 42, "tokens=100000"), &root.0),
        0
    );
    let origin = root.the_session();

    let wal = SessionWal::open(&root.checkpoint_db(&origin)).expect("wal");
    let commits = wal.commits(&origin).expect("commits");
    assert!(!commits.is_empty(), "the happy run commits every boundary");
    let first = commits.first().expect("first commit").seq;

    let args = || cmd::trace::ReplayArgs {
        session_id: origin.to_string(),
        at: Some(first.0),
        budget: None,
    };
    let first_code = cmd::trace::replay(args(), &root.0);
    assert_eq!(first_code, 0, "the first replay completes");

    // Second identical replay: same fork id (deterministic derivation) — must
    // regenerate, not refuse.
    let second_code = cmd::trace::replay(args(), &root.0);
    assert_eq!(
        second_code, 0,
        "the second identical replay regenerates the stale fork and completes"
    );

    // The regenerated fork still carries the verified lineage and the same structure.
    let fork = derive_fork_session_id(&origin, first);
    let fork_row = SessionWal::open(&root.checkpoint_db(&fork))
        .expect("fork wal")
        .session_row(&fork)
        .expect("row")
        .expect("fork row exists");
    assert_eq!(
        fork_row.fork_of.as_deref(),
        Some(format!("{origin}#{}", first.0)).as_deref(),
        "the regenerated fork re-records its lineage"
    );
    let origin_events = root.events(&origin);
    let fork_events = root.events(&fork);
    let cmp = compare_structure(
        &project_structure(&origin_events),
        &project_structure(&fork_events),
    );
    assert!(
        cmp.matches,
        "a completed whole-shape replay at #1 diverges only past the fork point: {:?}",
        cmp.mismatches.first()
    );
}
