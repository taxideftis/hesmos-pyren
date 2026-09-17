//! Dev server for the dashboard + API — stands in for `hesmos serve` until
//! the real core adapter exists. Boots the seeded stub core so Chromium E2E /
//! axe checks can run against real screens with representative data:
//!
//!     cargo run -p gateway --bin devserve -- [--bind 127.0.0.1] [--port 7330]
//!
//! Flags/defaults/banners are the CLI-6 surface itself (shared parser in
//! [`gateway::serve`]); only the core behind it differs (seed fixture vs the
//! production adapter `hesmos serve` will gain). Exit codes mirror CLI-6:
//! 0 graceful shutdown, 2 usage, 3 bind failure.

use std::process::ExitCode;
use std::sync::Arc;

#[tokio::main]
async fn main() -> ExitCode {
    // argv[1] is already the first flag (no `serve` subcommand here) → skip 1.
    let (bind, port) = match gateway::serve::parse_serve_flags(std::env::args().skip(1)) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("✗ USAGE-ARGS — {msg}");
            eprintln!("  사용법: devserve [--bind <addr>] [--port <n>]  (기본 127.0.0.1:7330)");
            return ExitCode::from(2);
        }
    };

    // Seeded stub core — deleted once the real hesmos-core adapter lands
    // (see gateway/src/stub.rs).
    let core = Arc::new(gateway::stub::StubCore::seeded());

    let running = match gateway::serve::bind(bind, port, core).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ serve 기동 실패 — {e} (exit 3)");
            return ExitCode::from(3);
        }
    };

    gateway::serve::warn_external_bind(bind);
    gateway::serve::print_banner(&running.local_url());
    match running.serve_until_shutdown().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("✗ serve 오류 — {e}");
            ExitCode::from(3)
        }
    }
}
