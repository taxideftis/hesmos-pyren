//! HTTP-3 `WS /api/sessions/{id}/stream` — real-time event streaming.
//!
//! This is the EventSink's opt-in fan-out realized as a transport: each WS
//! connection subscribes at connect time and receives only events emitted
//! **after** the subscription point (no backfill — catch-up is the client's
//! HTTP-2 `?after=<seq>` job, which is always safe because the trace is an
//! event-sourced log; A-N5). It is an explicit-subscription response channel,
//! not telemetry (P7 untouched).
//!
//! The WS endpoint stays inside the `/api` namespace (contract naming rule —
//! a separate `/ws` prefix was rejected by the contract).

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use tokio::sync::broadcast;

use crate::core_port::{CorePort, TraceEventWire};
use crate::error::ApiError;

pub async fn ws(
    Path(id): Path<String>,
    State(core): State<Arc<dyn CorePort>>,
    ws: WebSocketUpgrade,
) -> Response {
    // Unknown session → contract error body with 404, before the upgrade.
    let rx = match core.subscribe(&id) {
        Ok(rx) => rx,
        Err(e) => return ApiError(e).into_response(),
    };
    ws.on_upgrade(move |socket| forward(socket, rx))
}

async fn forward(mut socket: WebSocket, mut rx: broadcast::Receiver<TraceEventWire>) {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let Ok(text) = serde_json::to_string(&ev) else {
                    break;
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break; // client went away — stop streaming
                }
            }
            // Lagged = the subscriber missed events. Closing makes the client
            // run its "full re-sync then re-subscribe" path (?after=<seq>),
            // which is the contract-safe recovery (A-N5).
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
