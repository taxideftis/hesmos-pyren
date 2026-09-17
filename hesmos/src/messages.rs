//! The user-facing message catalog — one file, one tone (code-structure §3; P0a's
//! `cmd::not_implemented` promised this catalog would replace per-command ad-hoc
//! strings).
//!
//! Message tone contract (exceptions.md §9-4, applies to human output too): cause +
//! next action — a bare "failed" is forbidden. Language: Korean (the mission's user
//! layer); identifiers, codes and states stay in their canonical English spellings.

use hesmos_core::{CompileError, ReasonCode};

/// CLI usage guidance for the `--budget` spec grammar (CE-09's `--budget key=val`).
pub const BUDGET_SPEC_HELP: &str =
    "--budget 스펙은 key=val 나열이어야 합니다: tokens=250000 또는 tokens=unbounded";

pub fn session_not_found(session_id: &str) -> String {
    format!("세션을 찾을 수 없습니다: {session_id} — `hesmos run`이 만든 세션 ID인지 확인하세요")
}

pub fn not_commit_point(session_id: &str, seq: u64, valid: &[u64]) -> String {
    format!(
        "commit #{seq}은(는) {session_id}의 커밋 지점이 아닙니다 (유효한 지점: {valid:?}) — `trace show`로 커밋 목록을 확인하세요"
    )
}

pub fn evidence_invalid(detail: &str) -> String {
    format!("증거 불능: 로컬 체인 검증 실패 — {detail} (이 세션의 trace는 tamper 의심 상태입니다)")
}

pub fn bathos_absent() -> String {
    "bathos 엔진을 찾을 수 없어 audit 결합을 보류했습니다 (audit_verified: 보류) — bathos 설치 후 재시도하세요".to_string()
}

pub fn budget_spec_malformed(spec: &str) -> String {
    format!("--budget 스펙이 잘못되었습니다: `{spec}` — {BUDGET_SPEC_HELP}")
}

pub fn executor_unsupported(name: &str) -> String {
    format!(
        "HESMOS_EXECUTOR={name}은(는) 지원되지 않습니다 — P1은 `echo` 실행기만 제공합니다 (Python 브리지는 PY-1..5에서 연결)"
    )
}

/// bathos의 모델 판정 거부 — 코드는 bathos 원문 그대로노출(exceptions.md §5 치환 금지).
pub fn model_refused(code: &str) -> String {
    format!(
        "bathos 모델 검증이 세션 개시를 거부했습니다 [{code}] — 플랜의 model_ref가 bathos 모델 레지스트리에 있는지 확인하세요 (판정 코드는 bathos 원문입니다)"
    )
}

pub fn plan_unreadable(path: &str, cause: &str) -> String {
    format!("계획 파일을 읽을 수 없습니다: {path} — {cause} — 경로와 읽기 권한을 확인하세요")
}

/// CE-* render: code + Debug body + next action. The Debug form is core's sanctioned
/// formatting (CompileError deliberately has no Display; HesmosError::compile does the
/// same), so the human line and the stderr JSON line never disagree on the details.
pub fn compile_failed(err: &CompileError) -> String {
    format!(
        "계획 컴파일 실패 [{}] — {:?} — 계획 YAML의 해당 위치를 수정한 뒤 다시 실행하세요",
        err.code(),
        err
    )
}

/// The reason-code catalog (한국어, reason 6종) — 무엇이+왜+어떻게. Exit summaries and
/// the suspend/halt guidance blocks share these spellings so the same failure always
/// reads the same way.
pub fn reason_message(code: ReasonCode) -> String {
    match code {
        ReasonCode::BUDGET_EXCEEDED => {
            "BUDGET_EXCEEDED — 예산 봉투의 suspend 선에 도달했습니다 — 마지막 커밋 지점에서 재개하세요: hesmos trace replay <id> --at step <N> --budget tokens=<상한>".to_string()
        }
        ReasonCode::MAX_HANDOFFS => {
            "MAX_HANDOFFS — 누적 핸드오프가 상한(기본 20)을 넘었습니다 — 이전 커밋 지점에서 분기 재실행하세요: hesmos trace replay <id> --at step <N>".to_string()
        }
        ReasonCode::REPETITIVE_HANDOFF => {
            "REPETITIVE_HANDOFF — 관측 윈도우 내 A→B→A 핑퐁이 감지됐습니다 — 이전 커밋 지점에서 분기 재실행하세요: hesmos trace replay <id> --at step <N>".to_string()
        }
        ReasonCode::TIMEOUT => {
            "TIMEOUT — 외부 의존이 시간 안에 회신하지 않았습니다 — 이전 커밋 지점에서 재실행하거나 실행기 상태를 확인하세요".to_string()
        }
        ReasonCode::PROVIDER_FAILURE => {
            "PROVIDER_FAILURE — 실행기(프로바이더)가 반복 실패했습니다 — 이전 커밋 지점에서 재실행하거나 실행기 상태를 확인하세요".to_string()
        }
        ReasonCode::GATE_REJECT => {
            "GATE_REJECT — 게이트 판정이 최종 기각했습니다 — hesmos trace show <id> --gate 으로 사유 점수를 진단한 뒤 계획·입력을 수정해 재실행하세요".to_string()
        }
    }
}

/// SIGINT suspend guidance — the full re-pin command (W3-6: resume 안내는
/// `--budget` 재고정까지 포함한 완성형 명령).
pub fn suspended_resume(session_id: &str, last_commit: Option<u64>) -> String {
    match last_commit {
        Some(n) => format!(
            "재개: hesmos trace replay {session_id} --at step {n} --budget tokens=<상한> (마지막 커밋 지점)"
        ),
        None => format!(
            "재개: hesmos trace replay {session_id} --at step <N> --budget tokens=<상한> — 아직 커밋 지점이 없습니다 (첫 커밋 후 재개 가능)"
        ),
    }
}

pub fn eval_suite_unreadable(path: &str, cause: &str) -> String {
    format!(
        "eval 수트를 읽을 수 없습니다: {path} — {cause} — eval/suites/ 아래의 수트 파일을 확인하세요"
    )
}

pub fn eval_suite_invalid(path: &str, cause: &str) -> String {
    format!(
        "eval 수트 스키마가 올바르지 않습니다: {path} — {cause} — name·cases(id·session) 형식으로 수정하세요"
    )
}

pub fn eval_duplicate_case(suite: &str, case_id: &str) -> String {
    format!(
        "eval 수트 `{suite}`에 case id `{case_id}`가 중복됩니다 — case id는 골든 샘플의 키이므로 유일해야 합니다"
    )
}

pub fn eval_golden_missing(case_id: &str, session: &str) -> String {
    format!(
        "case `{case_id}`의 골든 샘플이 없습니다 — 먼저 승인하세요: hesmos eval <수트> --bless {session}"
    )
}

pub fn eval_bless_no_case(session: &str, suite: &str) -> String {
    format!(
        "수트 `{suite}`에 원본 세션이 {session}인 case가 없습니다 — 수트의 session 값을 확인하세요"
    )
}

pub fn eval_bless_ambiguous(session: &str, suite: &str) -> String {
    format!(
        "수트 `{suite}`에 원본 세션이 {session}인 case가 여러 개입니다 — 승인 대상을 특정할 수 없으므로 case별 세션을 분리하세요"
    )
}
