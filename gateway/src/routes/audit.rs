//! HTTP-5 `GET /api/audit/{session_id}` — seal + verify pass-through.
//!
//! Sourcing rule (HTTP-5): the verify bool is asked for by the core (PORT-2
//! `audit_verify`) and handed over verbatim. The gateway never touches bathos
//! and never recomputes the local chain (SS-24 rules 1–2, P8).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::core_port::{CoreError, CorePort};
use crate::error::ApiError;

pub async fn verify(Path(id): Path<String>, State(core): State<Arc<dyn CorePort>>) -> Response {
    match core.audit(&id) {
        // Not sealed (or unknown id) → 404 "없음", per contract.
        Ok(Some(status)) => Json(status).into_response(),
        Ok(None) => ApiError(CoreError::NotFound(id)).into_response(),
        Err(e) => ApiError(e).into_response(),
    }
}
