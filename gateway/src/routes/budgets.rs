//! HTTP-4 `GET /api/budgets?team_id={team_id}` — 3-unit aggregate.
//!
//! The response IS the core's aggregation (`hesmos budget --team`과 동일
//! 산출). The gateway neither recomputes sums nor merges ledgers — the
//! passthrough equality is asserted by an E2E test that compares the route
//! output byte-for-byte with the port's report.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::core_port::CorePort;
use crate::error::ApiError;

pub async fn team(
    Query(q): Query<HashMap<String, String>>,
    State(core): State<Arc<dyn CorePort>>,
) -> Response {
    // Absent/empty team_id = aggregate across all teams (a filter, not an
    // error); an unknown team yields an empty aggregate, which D3 renders as
    // its empty state.
    let team_id = q
        .get("team_id")
        .map(String::as_str)
        .filter(|s| !s.is_empty());
    match core.budgets(team_id) {
        Ok(report) => Json(report).into_response(),
        Err(e) => ApiError(e).into_response(),
    }
}
