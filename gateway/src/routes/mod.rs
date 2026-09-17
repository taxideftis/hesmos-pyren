//! Route table — the complete HTTP surface (HTTP-1~5 + the 4 dashboard
//! screens + two static assets). Nothing else is routed: "4화면 밖 라우트
//! 부재" is a story-level boundary test, and every route here is GET-only
//! (read-only dashboard, ui-spec §9.0).

mod audit;
mod budgets;
mod sessions;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use crate::core_port::CorePort;
use crate::{stream, ui};

pub fn router(core: Arc<dyn CorePort>) -> Router {
    Router::new()
        // HTTP-1
        .route("/api/sessions", get(sessions::list))
        // HTTP-2 snapshot + events
        .route("/api/sessions/{id}", get(sessions::snapshot))
        .route("/api/sessions/{id}/events", get(sessions::events))
        // HTTP-3
        .route("/api/sessions/{id}/stream", get(stream::ws))
        // HTTP-4
        .route("/api/budgets", get(budgets::team))
        // HTTP-5
        .route("/api/audit/{id}", get(audit::verify))
        // Dashboard D1–D4 (read-only, server-rendered)
        .route("/", get(ui::d1))
        .route("/sessions/{id}", get(ui::d2))
        .route("/budgets", get(ui::d3))
        .route("/audit", get(ui::d4))
        // Single CSS + minimal WS JS (ui-spec §9.0 technical level)
        .route("/assets/hesmos.css", get(ui::css))
        .route("/assets/hesmos.js", get(ui::js))
        .with_state(core)
}
