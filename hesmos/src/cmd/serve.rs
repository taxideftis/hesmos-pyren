//! CLI-6 `hesmos serve` — the read-only dashboard + HTTP-1~5 API gateway.
//!
//! This module is a **pure dispatcher** (code-structure §4): it parses the
//! two-flag CLI-6 surface, boots the gateway's blocking entry point, and maps
//! outcomes onto the exit-code map. Everything else — sockets, routes, UI,
//! shutdown — belongs to the `gateway` crate (D-4/SS-24: the CLI maps onto the
//! gateway's public API exclusively; never gateway internals, never
//! Python/FFI).
//!
//! Exit mapping (exceptions.md §8 via `crate::exit` — exit numbers live only
//! there):
//! - `EXIT_OK` (0)      — graceful shutdown on SIGINT/SIGTERM
//! - `EXIT_USAGE` (2)   — malformed flags (USAGE-ARGS)
//! - `EXIT_COMPILE` (3) — bind failure (port taken) / serve error
//!
//! Argument note: `dispatch()` keeps Phillip's P0a seat signature (no args),
//! reading `env::args().skip(2)`. `main.rs`'s `Serve` variant currently
//! declares no flag fields, so clap rejects `--bind/--port` before dispatch
//! runs; `hesmos serve` (defaults) works end-to-end today. Wiring note for
//! WP-P1e: give the variant `--bind/--port` fields and pass them here (or let
//! this parser see them) — 4 lines in main.rs, then flags light up unchanged.
//!
//! ponytail: the served core is `gateway::stub::StubCore::empty()` — an
//! honest empty store, since the real CorePort adapter has nothing to consume
//! yet (hesmos-orchestrator's WAL/session-store query API is a WP-P1e
//! remainder). `hesmos serve` therefore boots fully (banner, screens, exit
//! codes) and shows the designed empty state. Upgrade path: swap this one
//! constructor for the adapter over the orchestrator's read surface; trigger:
//! hesmos-orchestrator exposes session listing / trace query / budget
//! aggregate / audit_verify (see gateway/src/stub.rs for the same trigger).

use std::sync::Arc;

use crate::exit::{EXIT_COMPILE, EXIT_OK, EXIT_USAGE};

// `pub` (was P0a's `pub(crate)`): the binary now dispatches through the library
// (lib/bin split), so the seat must be visible across the crate boundary.
pub fn dispatch() -> i32 {
    let usage = |msg: &str| -> i32 {
        eprintln!("✗ USAGE-ARGS — {msg}");
        eprintln!("  사용법: hesmos serve [--bind <addr>] [--port <n>]  (기본 127.0.0.1:7330)");
        EXIT_USAGE
    };

    // Skip argv[0] (binary) and argv[1] (`serve`); the rest are our flags.
    let (bind, port) = match gateway::serve::parse_serve_flags(std::env::args().skip(2)) {
        Ok(v) => v,
        Err(msg) => return usage(&msg),
    };

    // See ponytail note: honest empty store until the real adapter exists.
    let core: Arc<dyn gateway::core_port::CorePort> = Arc::new(gateway::stub::StubCore::empty());

    match gateway::serve::run_blocking(bind, port, core) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("✗ serve 기동 실패 — {e}");
            EXIT_COMPILE
        }
    }
}
