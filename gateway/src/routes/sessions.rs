//! HTTP-1 `GET /api/sessions` and HTTP-2 snapshot/events.
//!
//! Handlers are pure mappings: request → `CorePort` query → contract-shaped
//! JSON. No caching, no state (SS-24 rule 2).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::core_port::{CoreError, CorePort, EVENTS_LIMIT_MAX};
use crate::error::ApiError;

/// HTTP-1 — session list via core SessionHandles.
pub async fn list(State(core): State<Arc<dyn CorePort>>) -> Response {
    match core.list_sessions() {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => ApiError(e).into_response(),
    }
}

/// HTTP-2 — SessionHandle snapshot; 404 with the contract error body when
/// the id is unknown.
pub async fn snapshot(Path(id): Path<String>, State(core): State<Arc<dyn CorePort>>) -> Response {
    match core.session(&id) {
        Some(s) => Json(s).into_response(),
        None => ApiError(CoreError::NotFound(id)).into_response(),
    }
}

/// HTTP-2 — events with mandatory pagination: `?after=<seq>&limit<=500`.
///
/// Query parsing is manual (not `Query<T>` deserialization) so malformed
/// values produce the contract error body `{"error": HesmosError}` instead of
/// axum's default rejection text.
pub async fn events(
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    State(core): State<Arc<dyn CorePort>>,
) -> Response {
    let after = match parse_after(&q) {
        Ok(v) => v,
        Err(e) => return ApiError(e).into_response(),
    };
    let limit = match parse_limit(&q) {
        Ok(v) => v,
        Err(e) => return ApiError(e).into_response(),
    };
    match core.events(&id, after, limit) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => ApiError(e).into_response(),
    }
}

/// `after` — exclusive cursor on `seq`. Absent or empty means "from the very
/// beginning" (`None`, includes seq 0); a numeric value returns `seq > after`
/// so `?after=<last seen>` never replays. Distinguishing absent from `after=0`
/// is deliberate: the initial D2 load (`?after=`) must include session.open.
fn parse_after(q: &HashMap<String, String>) -> Result<Option<u64>, CoreError> {
    match q.get("after").map(String::as_str) {
        None | Some("") => Ok(None),
        Some(raw) => raw.parse::<u64>().map(Some).map_err(|_| {
            CoreError::Usage(format!(
                "after 값 '{raw}'을(를) seq 번호로 해석할 수 없습니다"
            ))
        }),
    }
}

/// `limit` — absent means the contract maximum (500); 0 and >500 are Usage.
fn parse_limit(q: &HashMap<String, String>) -> Result<u16, CoreError> {
    match q.get("limit") {
        None => Ok(EVENTS_LIMIT_MAX),
        Some(raw) => {
            let v = raw.parse::<u16>().map_err(|_| {
                CoreError::Usage(format!("limit 값 '{raw}'을(를) 해석할 수 없습니다"))
            })?;
            if v == 0 {
                return Err(CoreError::Usage("limit은 1 이상이어야 합니다".into()));
            }
            if v > EVENTS_LIMIT_MAX {
                return Err(CoreError::Usage(format!(
                    "limit {v}은(는) 상한 {EVENTS_LIMIT_MAX}을(를) 초과합니다 — unbounded 조회는 금지됩니다"
                )));
            }
            Ok(v)
        }
    }
}
