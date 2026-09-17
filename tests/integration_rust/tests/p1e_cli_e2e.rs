//! WP-P1e E2E — the CLI surface driven in-process, exactly the code the binary
//! dispatches through (T7 + T11 + the CLI-level S1 check).
//!
//! Scenarios per the story's §8: dry-run → run → fault injection → trace show →
//! replay --at → budget; the FULL exit-band snapshot (US-07 AC1 — all nine values
//! asserted from real scenarios); T11 seal → bathos audit via a stub `bathos`
//! executable, with post-seal tamper detection.
//!
//! Environment discipline: `run`/`replay` read HESMOS_EXECUTOR / HESMOS_POLICY_YAML /
//! HESMOS_BATHOS. Env is process-global, so every test that touches it holds
//! [`ENV_LOCK`] — tests in this file would otherwise race each other's variables.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hesmos::cmd;
use hesmos_budget::Ledger;
use hesmos_core::{EventKind, SessionId};
use hesmos_orchestrator::{SessionWal, derive_fork_session_id};
use hesmos_trace::load_path;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Serializes env-mutating tests (see module doc).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("hesmos-p1e-{name}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        Self(dir)
    }

    fn write_plan(&self, text: &str) -> PathBuf {
        let path = self.0.join("plan.hes");
        std::fs::write(&path, text).expect("write plan");
        path
    }

    /// The single session dir under this root — fresh roots make "the one session"
    /// unambiguous (tests that fork assert on the derived fork id instead).
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

/// Serializes env-sensitive tests (see module doc). Poison-resilient on purpose: a
/// panicked holder must not cascade PoisonErrors into every other test.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Resets every key the CLI reads — a panicked earlier test may have skipped its
/// own cleanup, and stale env would silently change the scenario under test.
fn clean_env() {
    clear_env("HESMOS_EXECUTOR");
    clear_env("HESMOS_POLICY_YAML");
    clear_env("HESMOS_BATHOS");
}

// SAFETY: env mutation is process-global in newer Rust editions; every call site
// MUST already hold [`env_lock`] (each scenario wraps set → run → clear in one
// guard), which makes the mutation single-threaded.
fn set_env(key: &str, value: &str) {
    // SAFETY: see above — caller holds env_lock.
    unsafe { std::env::set_var(key, value) };
}

/// SAFETY: caller holds env_lock — see [`set_env`].
fn clear_env(key: &str) {
    // SAFETY: see above — caller holds env_lock.
    unsafe { std::env::remove_var(key) };
}

/// The 4-node fan-out plan — same shape the manual smoke and the S1 test use.
const PLAN: &str = r#"
name: p1e-e2e
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
        team: Some("e2e-team".into()),
        dry_run: false,
    }
}

/// T7 ① — dry-run creates NOTHING (US-04 AC1); a real run completes, seals, and
/// writes every ERD §4 session artifact; the banner facts are recorded as events.
#[test]
fn t7_dry_run_then_full_run_artifacts_and_seal() {
    // Reads env (executor/bathos selection) even though it sets nothing — hold the
    // lock so a concurrent scenario's env window can't leak into this run.
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("full-run");
    let plan = root.write_plan(PLAN);

    // --dry-run: exit 0, and the root gains NO session storage at all.
    let code = cmd::run::execute(
        cmd::run::RunArgs {
            dry_run: true,
            ..run_args(&plan, 42, "tokens=100000")
        },
        &root.0,
    );
    assert_eq!(code, 0, "dry-run exits 0");
    assert!(!root.sessions_root().exists(), "dry-run creates no session");

    // Real run: COMPLETED (exit 0) with a sealed chain.
    let code = cmd::run::execute(run_args(&plan, 42, "tokens=100000"), &root.0);
    assert_eq!(code, 0, "happy-path run exits 0");

    let session = root.the_session();
    let events = root.events(&session);

    // Chain shape: opens with the reproduction facts, closes COMPLETED, seals last.
    assert_eq!(events.first().expect("open").kind, EventKind::SessionOpen);
    assert_eq!(
        events.first().expect("open").attrs.get_u64("seed"),
        Some(42),
        "the pinned seed is recorded on session.open"
    );
    assert_eq!(
        events
            .iter()
            .find(|e| e.kind == EventKind::SessionClose)
            .expect("close")
            .attrs
            .get_str("final_state"),
        Some("COMPLETED")
    );
    let last = events.last().expect("seal");
    assert_eq!(last.kind, EventKind::TraceSeal, "completed sessions seal");
    assert!(
        events.iter().any(|e| e.kind == EventKind::HandoffAccept),
        "the fan-out routed (4 handoffs total)"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::HandoffRequest)
            .count(),
        4,
        "fetch fans out ×2, summarize/verify/done ×1 each"
    );

    // ERD §4 artifacts: events.jsonl (read above), checkpoint.db, plan snapshot,
    // response cache.
    let dir = root.sessions_root().join(session.to_string());
    assert!(dir.join("checkpoint.db").exists(), "WAL/ledger db exists");
    assert!(
        dir.join("plan_compiled.yaml").exists(),
        "plan snapshot exists"
    );
    assert!(dir.join("responses").exists(), "response cache dir exists");

    // US-20 AC3 — the ledger's session sum equals the metered llm.call tokens.
    let metered: u64 = events
        .iter()
        .filter(|e| e.kind == EventKind::LlmCall)
        .map(|e| {
            e.attrs.get_u64("tokens_in").unwrap_or(0) + e.attrs.get_u64("tokens_out").unwrap_or(0)
        })
        .sum();
    let ledger = Ledger::open(&session, &dir.join("checkpoint.db").display().to_string())
        .expect("ledger opens");
    assert_eq!(
        ledger.session_totals().expect("totals").total(),
        metered,
        "ledger sums == event sums"
    );
}

/// T7 ② — replay at a commit point forks into a NEW session whose structure equals
/// the origin's (CLI-level S1: session ids differ BY DESIGN — session.open carries
/// the id — so equality is structural: same event count, same kind/node sequence).
/// A non-commit point is a usage error and forks nothing.
#[test]
fn t7_replay_forks_structurally_and_rejects_non_commit_points() {
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("replay");
    let plan = root.write_plan(PLAN);
    assert_eq!(
        cmd::run::execute(run_args(&plan, 42, "tokens=100000"), &root.0),
        0,
        "origin completes"
    );
    let origin = root.the_session();

    // Four node boundaries → four commits.
    let wal = SessionWal::open(
        &root
            .sessions_root()
            .join(origin.to_string())
            .join("checkpoint.db"),
    )
    .expect("wal");
    let commits = wal.commits(&origin).expect("commits");
    assert_eq!(commits.len(), 4, "one commit per node boundary");

    // A point that never existed: usage band, and NO fork session appears.
    let code = cmd::trace::replay(
        cmd::trace::ReplayArgs {
            session_id: origin.to_string(),
            at: Some(999),
            budget: None,
        },
        &root.0,
    );
    assert_eq!(code, 2, "NOT-COMMIT-POINT is a usage error");
    assert_eq!(
        std::fs::read_dir(root.sessions_root())
            .expect("sessions")
            .count(),
        1,
        "a rejected replay forks nothing"
    );

    // Fork at commit 1: exit 0, derived id, fork_of lineage recorded.
    let code = cmd::trace::replay(
        cmd::trace::ReplayArgs {
            session_id: origin.to_string(),
            at: Some(1),
            budget: None,
        },
        &root.0,
    );
    assert_eq!(code, 0, "fork replay completes");
    let fork = derive_fork_session_id(&origin, hesmos_core::CommitSeq(1));
    assert!(
        root.sessions_root().join(fork.to_string()).exists(),
        "the fork session dir exists under the derived id"
    );
    let wal_fork = SessionWal::open(
        &root
            .sessions_root()
            .join(fork.to_string())
            .join("checkpoint.db"),
    )
    .expect("fork wal");
    let row = wal_fork
        .session_row(&fork)
        .expect("row")
        .expect("fork is registered");
    assert_eq!(
        row.fork_of.as_deref(),
        Some(format!("{origin}#1")).as_deref(),
        "fork_of names the origin and the fork point"
    );
    assert_eq!(row.seed, 42, "the fork inherits the origin's seed");

    // CLI-level S1: same seed + plan (+ same origin responses) → same structure.
    let a = root.events(&origin);
    let b = root.events(&fork);
    assert_eq!(a.len(), b.len(), "same event count");
    assert_eq!(shape(&a), shape(&b), "kind+node sequence identical");
}

/// Structural fingerprint for the CLI-level S1 comparison: (kind, node) per event.
fn shape(events: &[hesmos_core::TraceEvent]) -> Vec<(EventKind, Option<String>)> {
    events
        .iter()
        .map(|e| (e.kind, e.node.as_ref().map(|n| n.as_str().to_string())))
        .collect()
}

/// US-07 AC1 — the exit-band snapshot: each of the nine bands asserted from a REAL
/// scenario end to end (the story's 성문화 requirement). Env-touching scenarios run
/// under [`ENV_LOCK`].
#[test]
fn t7_exit_band_snapshot_all_nine_values() {
    let _env = env_lock();
    clean_env();

    // 0 — ok (a completed run; also covered by every happy-path test here).
    {
        let root = TempRoot::new("band-0");
        let plan = root.write_plan(PLAN);
        assert_eq!(
            cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0),
            0
        );
    }

    // 2 — usage (non-commit replay point).
    {
        let root = TempRoot::new("band-2");
        let plan = root.write_plan(PLAN);
        assert_eq!(
            cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0),
            0
        );
        let origin = root.the_session();
        assert_eq!(
            cmd::trace::replay(
                cmd::trace::ReplayArgs {
                    session_id: origin.to_string(),
                    at: Some(77),
                    budget: None
                },
                &root.0
            ),
            2
        );
    }

    // 3 — compile (schema-broken plan), and it leaves NO session behind (US-04 AC2).
    {
        let root = TempRoot::new("band-3");
        let plan = root.write_plan("name: broken\nflow: \"nonexistent -> also_missing\"\n");
        assert_eq!(
            cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0),
            3
        );
        assert!(!root.sessions_root().exists());
    }

    // 10 — budget suspend at the frozen envelope's suspend line.
    {
        let root = TempRoot::new("band-10");
        let plan = root.write_plan(PLAN);
        assert_eq!(
            cmd::run::execute(run_args(&plan, 7, "tokens=100"), &root.0),
            10
        );
        let session = root.the_session();
        let close = root
            .events(&session)
            .into_iter()
            .find(|e| e.kind == EventKind::SessionClose)
            .expect("close event");
        assert_eq!(close.attrs.get_str("final_state"), Some("SUSPENDED"));
    }

    // 11 — loop halt: max_handoffs=1 halts on the second routed handoff.
    {
        let root = TempRoot::new("band-11");
        let plan = root.write_plan(PLAN);
        set_env("HESMOS_POLICY_YAML", "max_handoffs: 1");
        let code = cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0);
        clear_env("HESMOS_POLICY_YAML");
        assert_eq!(code, 11);
        let session = root.the_session();
        let close = root
            .events(&session)
            .into_iter()
            .find(|e| e.kind == EventKind::SessionClose)
            .expect("close event");
        assert_eq!(close.attrs.get_str("final_state"), Some("HALTED"));
    }

    // 12 — provider halt: flaky:3 exhausts the bounded retry budget.
    {
        let root = TempRoot::new("band-12");
        let plan = root.write_plan(PLAN);
        set_env("HESMOS_EXECUTOR", "flaky:3");
        let code = cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0);
        clear_env("HESMOS_EXECUTOR");
        assert_eq!(code, 12);
    }

    // 20 — gate reject: low-confidence contracts + a raised floor exhaust the
    // bounded route retries → FAILED.
    {
        let root = TempRoot::new("band-20");
        let plan = root.write_plan(PLAN);
        set_env("HESMOS_EXECUTOR", "reject");
        set_env("HESMOS_POLICY_YAML", "min_confidence: 0.5");
        let code = cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0);
        clear_env("HESMOS_EXECUTOR");
        clear_env("HESMOS_POLICY_YAML");
        assert_eq!(code, 20);
    }

    // 30 — evidence invalid: a post-seal tamper is detected on inspection.
    {
        let root = TempRoot::new("band-30");
        let plan = root.write_plan(PLAN);
        assert_eq!(
            cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0),
            0
        );
        let session = root.the_session();
        let path = root
            .sessions_root()
            .join(session.to_string())
            .join("events.jsonl");
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::write(&path, text.replace("\"seed\":7", "\"seed\":9")).expect("tamper");
        assert_eq!(
            cmd::trace::show(
                cmd::trace::ShowArgs {
                    session_id: session.to_string(),
                    gate: false,
                    handoff: false,
                    limit: 200,
                    json: false,
                },
                &root.0
            ),
            30,
            "the tampered trace refuses to pass as evidence"
        );
    }

    // 130 — SIGINT band. The signal path itself is the ctrlc handler flipping the
    // runner's interrupt flag (covered by runner-level tests); here the CLI's
    // mapping law is pinned: a reason-less suspend IS the SIGINT band.
    assert_eq!(hesmos::composition::reason_exit(None, true), 130);
}

/// T7 ③ — CONCERNS: a failed first attempt that passes on retry is a concern, not a
/// failure — the session still COMPLETES with the retry visible in the chain.
#[test]
fn t7_concerns_retry_then_pass_completes() {
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("concerns");
    let plan = root.write_plan(PLAN);
    set_env("HESMOS_EXECUTOR", "concerns");
    let code = cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0);
    clear_env("HESMOS_EXECUTOR");
    assert_eq!(code, 0, "retry-then-pass completes");

    let session = root.the_session();
    let events = root.events(&session);
    assert!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::GateFail)
            .count()
            >= 1,
        "the first-attempt rubric failure is recorded"
    );
    assert_eq!(
        events
            .iter()
            .find(|e| e.kind == EventKind::SessionClose)
            .expect("close")
            .attrs
            .get_str("final_state"),
        Some("COMPLETED"),
        "a concern never fails the session by itself"
    );
}

/// T11 — the bathos audit join, end to end through the REAL subprocess adapter:
/// a stub `bathos` executable records `audit append --target <head>` and answers
/// `audit verify`. After the seal, the audit ledger's last entry IS the chain's
/// trace.seal head; a post-seal tamper flips local inspection to exit 30; a stub
/// that fails verify fails the seal (never "sealed anyway").
#[test]
fn t11_seal_joins_bathos_audit_and_tamper_is_detected() {
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("t11");

    // The stub: append writes the --target hash into ledger.txt; verify exits 0.
    let ledger = root.0.join("audit-ledger.txt");
    let ledger_path = ledger.display().to_string();
    let stub_bin = root.0.join("bathos-stub");
    std::fs::write(
        &stub_bin,
        format!(
            "#!/bin/bash\n\
             if [ \"$1 $2\" = \"audit append\" ]; then\n\
             \x20 target=\"\"\n\
             \x20 while [ $# -gt 0 ]; do if [ \"$1\" = \"--target\" ]; then target=\"$2\"; fi; shift; done\n\
             \x20 echo \"$target\" >> \"{ledger_path}\"\n\
             \x20 exit 0\n\
             fi\n\
             if [ \"$1 $2\" = \"audit verify\" ]; then echo '{{\"ok\":true}}'; exit 0; fi\n\
             exit 3\n",
            ledger_path = ledger_path
        ),
    )
    .expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&stub_bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    set_env("HESMOS_BATHOS", &stub_bin.display().to_string());

    let plan = root.write_plan(PLAN);
    let code = cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0);
    clear_env("HESMOS_BATHOS");
    assert_eq!(code, 0, "sealed + audit-joined run exits 0");

    // The audit ledger received exactly the head the seal event names.
    let session = root.the_session();
    let events = root.events(&session);
    let seal = events.last().expect("seal last");
    assert_eq!(seal.kind, EventKind::TraceSeal);
    let head = seal.attrs.get_str("chain_head_hash").expect("head attr");
    let submitted = std::fs::read_to_string(&ledger).expect("stub ran and recorded");
    assert_eq!(
        submitted.trim(),
        head,
        "audit_append carried exactly the trace.seal chain_head_hash"
    );

    // Tamper AFTER the seal: local inspection flips to evidence-invalid (exit 30).
    let events_path = root
        .sessions_root()
        .join(session.to_string())
        .join("events.jsonl");
    let text = std::fs::read_to_string(&events_path).expect("read");
    std::fs::write(&events_path, text.replace("score\":1", "score\":0.1")).expect("tamper");
    assert_eq!(
        cmd::trace::show(
            cmd::trace::ShowArgs {
                session_id: session.to_string(),
                gate: false,
                handoff: false,
                limit: 200,
                json: false,
            },
            &root.0
        ),
        30,
        "post-seal tamper is evidence-invalid"
    );

    // A bathos that REJECTS verify fails the seal: run exits 30, and the close
    // events carry no seal (never "sealed anyway" on a failed verify).
    let failing = root.0.join("bathos-stub-failing");
    std::fs::write(&failing, "#!/bin/bash\necho '{\"ok\":false}'\nexit 1\n").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let root2 = TempRoot::new("t11-verify-fail");
    let plan2 = root2.write_plan(PLAN);
    set_env("HESMOS_BATHOS", &failing.display().to_string());
    let code = cmd::run::execute(run_args(&plan2, 7, "tokens=100000"), &root2.0);
    clear_env("HESMOS_BATHOS");
    assert_eq!(code, 30, "a failed audit join is evidence-invalid");
    // The LOCAL seal stands even though the join failed (seal.rs order: the seal
    // event is written first, the audit submission follows) — the failed join is
    // reported through the exit band, not by pretending the seal never happened.
    let session2 = root2.the_session();
    let events2 = root2.events(&session2);
    assert!(
        events2.iter().any(|e| e.kind == EventKind::TraceSeal),
        "the local seal event stands"
    );
    assert_eq!(
        events2
            .iter()
            .find(|e| e.kind == EventKind::SessionClose)
            .expect("close")
            .attrs
            .get_str("final_state"),
        Some("COMPLETED"),
        "the session itself completed — only the EVIDENCE join failed"
    );
}

/// T7 ④ — budget CLI agrees with the ledger and the events (CLI-4 surface).
#[test]
fn t7_budget_query_reflects_ledger() {
    let _env = env_lock();
    clean_env();
    let root = TempRoot::new("budget");
    let plan = root.write_plan(PLAN);
    assert_eq!(
        cmd::run::execute(run_args(&plan, 7, "tokens=100000"), &root.0),
        0
    );
    let session = root.the_session();

    // Session-scope query exits 0; the numbers were proven == events in t7①.
    assert_eq!(
        cmd::budget::execute(
            cmd::budget::BudgetArgs {
                session_id: Some(session.to_string()),
                team: None,
                json: true,
            },
            &root.0
        ),
        0
    );
    // Team-scope aggregation across this root's sessions also resolves.
    assert_eq!(
        cmd::budget::execute(
            cmd::budget::BudgetArgs {
                session_id: None,
                team: Some("e2e-team".into()),
                json: false,
            },
            &root.0
        ),
        0
    );
    // A malformed session id is a usage error (exit 2); a WELL-FORMED id that was
    // never created is the exit-3 "session missing" band.
    assert_eq!(
        cmd::budget::execute(
            cmd::budget::BudgetArgs {
                session_id: Some("not-a-session-id".into()),
                team: None,
                json: false,
            },
            &root.0
        ),
        2
    );
    assert_eq!(
        cmd::budget::execute(
            cmd::budget::BudgetArgs {
                // Well-formed ULID, no session behind it.
                session_id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".into()),
                team: None,
                json: false,
            },
            &root.0
        ),
        3
    );
}
