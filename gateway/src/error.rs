//! HTTP error mapping — the common error body of HTTP-5:
//! `{"error": HesmosError}` with the TYPE-7 field set
//! (class, code, session_id?, node_id?, message, hint?).
//!
//! Status mapping is fixed by the contract: 400 Usage / 404 없음 / 409 상태
//! 충돌 / 500 내부. Class/code choices documented in
//! `.agent-team/08-impl-notes/frontend.md` (§ error vocabulary) — they reuse
//! the closed vocabularies of exceptions.md where one exists; the two codes
//! the CLI never needed (`SESSION_NOT_FOUND`, `E-INTERNAL`) are flagged there
//! for James's visibility instead of being silently invented.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::core_port::CoreError;

pub struct ApiError(pub CoreError);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, class, code, message, hint) = match &self.0 {
            CoreError::NotFound(id) => (
                StatusCode::NOT_FOUND,
                "Compile",
                "SESSION_NOT_FOUND",
                format!("세션 {id}의 trace를 찾을 수 없습니다"),
                "세션 id는 hesmos run 출력의 session 필드 또는 대시보드 D1 목록에서 확인하세요."
                    .to_string(),
            ),
            CoreError::Usage(msg) => (
                StatusCode::BAD_REQUEST,
                "Usage",
                "USAGE-ARGS",
                msg.clone(),
                "events 조회는 ?after=<seq>&limit<=500 형식을 사용하세요.".to_string(),
            ),
            CoreError::Conflict(msg) => (
                StatusCode::CONFLICT,
                "Usage",
                "USAGE-STATE",
                msg.clone(),
                "세션 상태가 바뀌었을 수 있습니다 — 목록을 새로고침하세요.".to_string(),
            ),
            CoreError::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Platform",
                "E-INTERNAL",
                msg.clone(),
                "다음: serve 로그를 확인하세요.".to_string(),
            ),
        };
        let body = json!({
            "error": {
                "class": class,
                "code": code,
                "message": message,
                "hint": hint,
            }
        });
        (status, Json(body)).into_response()
    }
}
