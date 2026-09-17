//! CLI-5 `hesmos eval` — the golden eval harness (WP-P3a, T10 core, S8 loop).
//!
//! A suite (`eval/suites/<name>.yaml`) lists cases; each case re-executes a recorded
//! session as a FORK — the same reproduction path `trace replay` drives (origin's
//! 4-elements, snapshot plan, response cache) — and compares the fork's STRUCTURE
//! (node path · gate verdicts) against the blessed golden sample. SS-22 rule 1: the
//! comparison is structural, never free text; a mismatch is the S8 CI regression
//! signal.
//!
//! Exit band (CLI-5 contract): pass → 0 · structural regression → 20 · suite/config/
//! evidence failure → 3. Error outranks Mismatch in the aggregation: an unevaluable
//! case means the suite said nothing, and a green-absent-red would be a false pass.
//!
//! `--bless <session_id>` is the ONLY approval surface: it runs the matching case
//! once and records the observed structure as the golden sample. Re-blessing an
//! unchanged sample is a byte-identical no-op (idempotent approval). `eval` itself
//! never writes a golden file — an unapproved suite fails with the bless hint; a
//! harness that self-approves would make the regression gate decorative.
//!
//! Judge bridges (P3b) are deliberately absent: the suite schema has no judge field
//! yet, and `deny_unknown_fields` keeps the surface honest until P3b lands it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use hesmos_core::{BudgetEnvelope, CommitSeq, ErrorClass, HesmosError};
use hesmos_orchestrator::{SessionWal, derive_fork_session_id};
use hesmos_trace::{SessionStructure, StructureMismatch, compare_structure, project_structure};

use crate::cmd::run::{
    clear_stale_fork, emit_error, load_events, load_fork_plan, open_session_runner,
};
use crate::cmd::trace::mismatch_line;
use crate::composition::{LogOnlyTee, parse_budget_spec, session_paths};
use crate::exit;
use crate::messages;

/// Everything the CLI-5 grammar contributes (`hesmos eval <suite> [--bless <id>] [--json]`).
#[derive(Debug, Clone)]
pub struct EvalArgs {
    /// Suite path or bare name (`demo` → `eval/suites/demo.yaml`).
    pub suite: PathBuf,
    pub bless: Option<String>,
    pub json: bool,
}

// ---------------------------------------------------------------------------
// Suite + golden schemas — the boundary types (deny_unknown_fields everywhere)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteFile {
    name: String,
    cases: Vec<CaseDef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseDef {
    id: String,
    /// The recorded (origin) session this case re-executes.
    session: String,
    /// Commit point to fork at; absent = 0 = whole-session replay.
    #[serde(default)]
    at: Option<u64>,
    /// Re-pin the fork envelope (`tokens=N`); absent = the origin's recorded budget.
    #[serde(default)]
    budget: Option<String>,
}

/// The golden sample file (`<suite>.golden.yaml`) — the ONLY eval artifact in the
/// repo, written exclusively by `--bless` (AP-6: derived views keep no other storage).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenFile {
    samples: BTreeMap<String, GoldenSample>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenSample {
    session: String,
    structure: SessionStructure,
}

// ---------------------------------------------------------------------------
// Case execution
// ---------------------------------------------------------------------------

enum CaseVerdict {
    /// Structure matched the golden sample.
    Pass,
    /// Structure diverged — the S8 regression signal (exit 20).
    Mismatch(Vec<StructureMismatch>),
    /// The case could not be evaluated (config/evidence); the message is the
    /// diagnosis. Empty message = the error line was already emitted downstream.
    Error(String),
}

struct CaseResult {
    id: String,
    session: String,
    verdict: CaseVerdict,
}

/// CLI-5 entry (library face — the binary dispatches through here).
pub fn execute(args: EvalArgs, root: &Path) -> i32 {
    let (code, report) = execute_captured(args, root);
    if let Some(text) = report {
        println!("{text}");
    }
    code
}

/// The same harness with the suite report RETURNED instead of printed — the E2E
/// harness drives this in-process and asserts on the text (stdout capture is not
/// available to it). Config errors still emit their stderr E-line immediately.
pub fn execute_captured(args: EvalArgs, root: &Path) -> (i32, Option<String>) {
    let suite_path = match resolve_suite_path(root, &args.suite) {
        Ok(p) => p,
        Err(msg) => return (config_error(msg), None),
    };
    let suite: SuiteFile = match read_suite(&suite_path) {
        Ok(s) => s,
        Err(msg) => return (config_error(msg), None),
    };

    // Boundary validation: case ids are the golden sample keys — duplicates would
    // silently merge approvals, so they never reach execution.
    let mut seen = std::collections::BTreeSet::new();
    for case in &suite.cases {
        if !seen.insert(case.id.clone()) {
            return (
                config_error(messages::eval_duplicate_case(&suite.name, &case.id)),
                None,
            );
        }
    }

    if let Some(bless_id) = &args.bless {
        return bless(&suite, &suite_path, bless_id, root, args.json);
    }

    let mut results = Vec::new();
    // The golden file is read once per suite invocation; a missing file simply means
    // every case reports "unapproved" (the bless hint), not a hard failure — partial
    // approvals (some cases blessed) are the normal mid-migration state.
    let golden = read_golden(&suite_golden_path(&suite_path)).ok();
    for case in &suite.cases {
        let expected = golden.as_ref().and_then(|g| g.samples.get(&case.id));
        let (session, verdict) = run_case(root, case, expected);
        results.push(CaseResult {
            id: case.id.clone(),
            session,
            verdict,
        });
    }
    let (code, text) = report(&suite.name, &results, args.json);
    (code, Some(text))
}

/// Runs one case: fork-execute the origin, project the fork's chain, compare to the
/// golden sample (when one is blessed — `None` = unapproved case, exit-3 with the
/// bless hint). Only a completed comparison can Pass or Mismatch.
fn run_case(root: &Path, case: &CaseDef, expected: Option<&GoldenSample>) -> (String, CaseVerdict) {
    let actual = match execute_case(root, case) {
        Ok(a) => a,
        Err(msg) => return (case.session.clone(), CaseVerdict::Error(msg)),
    };
    let Some(sample) = expected else {
        return (
            case.session.clone(),
            CaseVerdict::Error(format!(
                "case `{}` — {}",
                case.id,
                messages::eval_golden_missing(&case.id, &case.session)
            )),
        );
    };
    if sample.session != case.session {
        return (
            case.session.clone(),
            CaseVerdict::Error(format!(
                "case `{}` — 골든 샘플의 원본 세션이 수트와 불일치 (골든: {})",
                case.id, sample.session
            )),
        );
    }
    let cmp = compare_structure(&sample.structure, &actual);
    if cmp.matches {
        (case.session.clone(), CaseVerdict::Pass)
    } else {
        (case.session.clone(), CaseVerdict::Mismatch(cmp.mismatches))
    }
}

/// Executes one case's fork and projects the fork's chain — the execution half of a
/// case, shared by `eval` (compare) and `--bless` (record). `Err("")` means the
/// error line was already emitted downstream (the evidence/platform layers own
/// their stderr lines).
fn execute_case(root: &Path, case: &CaseDef) -> Result<SessionStructure, String> {
    let fail = |msg: String| Err(format!("case `{}` — {msg}", case.id));
    let silent = || Err(String::new());

    let Some(origin) = hesmos_orchestrator::parse_session_id(&case.session) else {
        return fail(format!("세션 ID 파싱 실패: `{}`", case.session));
    };
    let (_, events_path, checkpoint_db) = session_paths(root, &origin);
    if !events_path.exists() {
        return fail(messages::session_not_found(&case.session));
    }

    // The origin chain must be verified evidence before it forks anything (SS-02
    // rule 2). load_events already emitted the E-line on failure.
    if load_events(root, &origin).is_err() {
        return silent();
    }

    // Origin facts from the WAL (the reproduction basis).
    let wal = match SessionWal::open(&checkpoint_db) {
        Ok(w) => w,
        Err(e) => return fail(format!("원본 WAL open 실패: {e}")),
    };
    let row = match wal.session_row(&origin) {
        Ok(Some(r)) => r,
        Ok(None) => return fail(messages::session_not_found(&case.session)),
        Err(e) => return fail(format!("원본 세션 행 조회 실패: {e}")),
    };
    let commits = match wal.commits(&origin) {
        Ok(c) => c,
        Err(e) => return fail(format!("커밋 조회 실패: {e}")),
    };

    // The suite's commit point must exist (or be 0 = whole-session).
    let at = CommitSeq(case.at.unwrap_or(0));
    if at.0 != 0 && !commits.iter().any(|c| c.seq == at) {
        let valid: Vec<u64> = commits.iter().map(|c| c.seq.0).collect();
        return fail(messages::not_commit_point(&case.session, at.0, &valid));
    }

    let plan = match load_fork_plan(root, &origin, &row.plan_hash) {
        Ok(p) => p,
        Err(_) => return silent(),
    };

    let budget: BudgetEnvelope = match &case.budget {
        Some(spec) => parse_budget_spec(spec)?,
        None => serde_json::from_str(&row.budget_json).unwrap_or_default(),
    };

    let fork_session = derive_fork_session_id(&origin, at);
    // Deterministic fork id ⇒ a re-run lands on the previous fork dir; the lineage
    // guard regenerates it (idempotent eval) or refuses a foreign occupant.
    if clear_stale_fork(root, &origin, &fork_session, at).is_err() {
        return silent();
    }

    let runner = match open_session_runner(
        root,
        &plan,
        fork_session,
        row.seed,
        &budget,
        row.team_id
            .as_deref()
            .map(hesmos_orchestrator::parse_team_id),
        Some(hesmos_orchestrator::ForkSource {
            origin,
            at,
            origin_responses: session_paths(root, &origin).0.join("responses"),
        }),
        |log| Box::new(LogOnlyTee { log }),
    ) {
        Ok(r) => r,
        Err(_code) => return silent(), // the band line was already emitted upstream
    };
    if runner.run().is_err() {
        return fail("fork 실행 실패".into());
    }

    // Eval forks are THROWAWAY: never sealed (ST-1's seal is for run-completed
    // sessions) — their evidence obligation is this very comparison.
    let fork_events = match load_events(root, &fork_session) {
        Ok(e) => e,
        Err(_) => return silent(),
    };
    Ok(project_structure(&fork_events))
}

// ---------------------------------------------------------------------------
// Bless — the approval surface
// ---------------------------------------------------------------------------

/// `--bless <session_id>`: run the case bound to that origin session and record the
/// observed structure as its golden sample. Idempotent: an unchanged sample is a
/// byte-identical no-op (the file is rewritten only when approval changes it).
/// Returns (exit band, human report text — `None` under `--json`).
fn bless(
    suite: &SuiteFile,
    suite_path: &Path,
    bless_id: &str,
    root: &Path,
    json: bool,
) -> (i32, Option<String>) {
    let matching: Vec<&CaseDef> = suite
        .cases
        .iter()
        .filter(|c| c.session == bless_id)
        .collect();
    match matching.len() {
        0 => {
            return (
                config_error(messages::eval_bless_no_case(bless_id, &suite.name)),
                None,
            );
        }
        n if n > 1 => {
            return (
                config_error(messages::eval_bless_ambiguous(bless_id, &suite.name)),
                None,
            );
        }
        _ => {}
    }
    let case = matching[0];

    // Bless RUNS the case — the golden comes from an actual fork execution, never
    // from a hand-edited claim.
    let structure = match execute_case(root, case) {
        Ok(s) => s,
        Err(msg) => {
            if !msg.is_empty() {
                config_error(msg);
            }
            return (exit::EXIT_COMPILE, None);
        }
    };

    let golden_path = suite_golden_path(suite_path);
    let mut golden = read_golden(&golden_path).unwrap_or(GoldenFile {
        samples: BTreeMap::new(),
    });
    let sample = GoldenSample {
        session: case.session.clone(),
        structure,
    };
    let reapproved = golden
        .samples
        .get(case.id.as_str())
        .is_some_and(|old| old.structure != sample.structure);
    let unchanged = golden.samples.get(case.id.as_str()) == Some(&sample);
    if unchanged {
        let text = format!(
            "변경 없음 — case `{}`의 골든 샘플이 이미 동일합니다 (no-op)",
            case.id
        );
        return (exit::EXIT_OK, (!json).then_some(text));
    }
    golden.samples.insert(case.id.clone(), sample);
    let yaml = match serde_yaml_ng::to_string(&golden) {
        Ok(y) => y,
        Err(e) => return (config_error(format!("골든 파일 직렬화 실패: {e}")), None),
    };
    if let Err(e) = std::fs::write(&golden_path, yaml) {
        return (
            config_error(format!(
                "골든 파일 기록 실패 ({}): {e}",
                golden_path.display()
            )),
            None,
        );
    }
    let reapprove_note = if reapproved {
        "재승인 — 기존 골든과 다른 구조가 관측됐습니다 — 새 구조를 승인합니다\n"
    } else {
        ""
    };
    let text = format!(
        "{reapprove_note}승인 완료 — case `{}` 골든 샘플 기록 ({}) — 이후 eval이 이 구조를 기준으로 검사합니다",
        case.id,
        golden_path.display()
    );
    (exit::EXIT_OK, (!json).then_some(text))
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Builds the suite report (human block or one JSON object) and the exit band.
/// Pure — `execute_captured` decides where the text goes.
fn report(suite_name: &str, results: &[CaseResult], json: bool) -> (i32, String) {
    let any_error = results
        .iter()
        .any(|r| matches!(r.verdict, CaseVerdict::Error(_)));
    let mismatches = results
        .iter()
        .filter(|r| matches!(r.verdict, CaseVerdict::Mismatch(_)))
        .count();

    let exit_code = if any_error {
        exit::EXIT_COMPILE
    } else if mismatches > 0 {
        exit::EXIT_EVAL_REGRESSION
    } else {
        exit::EXIT_OK
    };

    if json {
        #[derive(Serialize)]
        struct JsonCase {
            id: String,
            session: String,
            status: &'static str,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            mismatches: Vec<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            message: Option<String>,
        }
        #[derive(Serialize)]
        struct JsonReport {
            suite: String,
            results: Vec<JsonCase>,
            exit: i32,
        }
        let report = JsonReport {
            suite: suite_name.to_string(),
            results: results
                .iter()
                .map(|r| JsonCase {
                    id: r.id.clone(),
                    session: r.session.clone(),
                    status: match r.verdict {
                        CaseVerdict::Pass => "pass",
                        CaseVerdict::Mismatch(_) => "mismatch",
                        CaseVerdict::Error(_) => "error",
                    },
                    mismatches: match &r.verdict {
                        CaseVerdict::Mismatch(m) => {
                            m.iter().map(|d| mismatch_line(d.clone())).collect()
                        }
                        _ => Vec::new(),
                    },
                    message: match &r.verdict {
                        CaseVerdict::Error(msg) if !msg.is_empty() => Some(msg.clone()),
                        _ => None,
                    },
                })
                .collect(),
            exit: exit_code,
        };
        let text = serde_json::to_string(&report).unwrap_or_else(|_| "{\"exit\":3}".into());
        return (exit_code, text);
    }

    let mut text = format!("eval suite {suite_name} — {} cases\n", results.len());
    for r in results {
        match &r.verdict {
            CaseVerdict::Pass => {
                text += &format!("  ✓ {} — 구조 일치 (원본 {})\n", r.id, r.session)
            }
            CaseVerdict::Mismatch(m) => {
                text += &format!("  ✗ {} — 구조 불일치 (원본 {})\n", r.id, r.session);
                for d in m {
                    text += &format!("      {}\n", mismatch_line(d.clone()));
                }
            }
            CaseVerdict::Error(msg) => {
                if msg.is_empty() {
                    text += &format!("  ! {} — 평가 불가 (원본 {})\n", r.id, r.session);
                } else {
                    text += &format!("  ! {} — {msg}\n", r.id);
                }
            }
        }
    }
    if any_error {
        text += "  verdict: 평가 불가 케이스 존재 — 수트·원본 세션 상태를 확인하세요 (exit 3)";
    } else if mismatches > 0 {
        text += &format!(
            "  verdict: 구조 회귀 {mismatches}건 — 골든이 옳다면 계획·게이트 변경이 원인입니다 (exit 20)"
        );
    } else {
        text += "  verdict: 전체 일치 (exit 0)";
    }
    (exit_code, text)
}

// ---------------------------------------------------------------------------
// File resolution + the exit-3 emitter
// ---------------------------------------------------------------------------

/// `<suite>` accepts a path as given, or a bare name under `eval/suites/` (with or
/// without the .yaml suffix). A missing suite is a config error (exit 3), not argv
/// misuse — CLI-5's contract band has no 2.
fn resolve_suite_path(root: &Path, given: &Path) -> Result<PathBuf, String> {
    if given.is_file() {
        return Ok(given.to_path_buf());
    }
    let candidates = [
        root.join("eval/suites").join(given),
        root.join("eval/suites").join(suitename_with_yaml(given)),
    ];
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        messages::eval_suite_unreadable(
            &given.display().to_string(),
            "파일을 찾을 수 없습니다 — eval/suites/ 아래의 수트 이름 또는 경로를 지정하세요",
        )
    })
}

fn suitename_with_yaml(given: &Path) -> PathBuf {
    let mut name = given.as_os_str().to_owned();
    name.push(".yaml");
    PathBuf::from(name)
}

fn read_suite(path: &Path) -> Result<SuiteFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        messages::eval_suite_unreadable(&path.display().to_string(), &e.to_string())
    })?;
    serde_yaml_ng::from_str(&text)
        .map_err(|e| messages::eval_suite_invalid(&path.display().to_string(), &e.to_string()))
}

fn read_golden(path: &Path) -> Result<GoldenFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        messages::eval_suite_unreadable(&path.display().to_string(), &e.to_string())
    })?;
    serde_yaml_ng::from_str(&text)
        .map_err(|e| messages::eval_suite_invalid(&path.display().to_string(), &e.to_string()))
}

fn suite_golden_path(suite_path: &Path) -> PathBuf {
    let stem = suite_path
        .file_stem()
        .map(|s| format!("{}.golden.yaml", s.to_string_lossy()))
        .unwrap_or_else(|| "suite.golden.yaml".into());
    suite_path.with_file_name(stem)
}

/// CLI-5's config/evidence band: every unevaluable condition exits 3 with an
/// EVAL-CONFIG line on stderr (exceptions.md §9-3 shape).
fn config_error(message: String) -> i32 {
    emit_error(&HesmosError {
        class: ErrorClass::Compile,
        code: "EVAL-CONFIG".into(),
        session_id: None,
        node_id: None,
        message,
        hint: None,
    });
    exit::EXIT_COMPILE
}
