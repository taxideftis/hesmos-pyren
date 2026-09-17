//! Transport bootstrap for CLI-6 `hesmos serve`.
//!
//! Split so the display layer (`hesmos/src/cmd/serve.rs`, Andrew) stays a pure
//! dispatcher while this module owns sockets:
//! 1. [`bind`] — bind the listener; `Err` maps to CLI exit 3 (기동 실패).
//! 2. [`print_banner`] / [`warn_external_bind`] — ui-spec §9.1 output duties.
//! 3. [`Running::serve_until_shutdown`] — serve until SIGTERM/SIGINT (exit 0).
//!
//! Loopback is the default bind (minimal privilege); anything else prints a
//! one-line warning per CLI-6.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::core_port::CorePort;
use crate::routes;

/// A bound, not-yet-serving server.
pub struct Running {
    pub local_addr: SocketAddr,
    handle: tokio::task::JoinHandle<io::Result<()>>,
}

/// Bind `bind:port` and prepare graceful shutdown. Fails (e.g. port taken)
/// before any output — the caller maps the error to exit 3.
pub async fn bind(bind: IpAddr, port: u16, core: Arc<dyn CorePort>) -> io::Result<Running> {
    let app = routes::router(core);
    let listener = tokio::net::TcpListener::bind((bind, port)).await?;
    let local_addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    });
    Ok(Running { local_addr, handle })
}

impl Running {
    /// `http://<addr>` URL for the banner.
    pub fn local_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// Serve until SIGTERM/SIGINT, then finish in-flight requests and return.
    /// Returning `Ok` corresponds to CLI exit 0 (우아한 종료).
    pub async fn serve_until_shutdown(self) -> io::Result<()> {
        self.handle.await.expect("serve task panicked")
    }
}

/// Resolves on the first of SIGINT (Ctrl+C) or SIGTERM.
async fn shutdown_signal() {
    use tokio::signal;
    #[cfg(unix)]
    {
        let mut term = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
    }
}

/// Startup banner — ui-spec §9.1 copy with the contract-confirmed endpoint
/// values (HTTP-1~5; A-N8). `?team_id=` (not the draft's `?team=`).
pub fn print_banner(url: &str) {
    println!("hesmos serve — {url}");
    println!("  세션 목록   GET /api/sessions                              HTTP-1  US-28 AC1");
    println!("  이벤트      GET /api/sessions/{{id}}/events                HTTP-2");
    println!("  스트리밍    WS  /api/sessions/{{id}}/stream                HTTP-3  US-28 AC2");
    println!("  예산·감사   GET /api/budgets?team_id= · /api/audit/{{id}}  HTTP-4·5  US-28 AC3");
    println!("  대시보드    /  (조회 전용)");
    println!("종료: Ctrl+C — 조회 전용이므로 코어 상태는 변경되지 않습니다");
}

/// CLI-6 minimal-privilege warning — exactly one line, only for non-loopback
/// binds.
pub fn warn_external_bind(bind: IpAddr) {
    if !bind.is_loopback() {
        println!("! 경고 — {bind}(으)로 바인딩하면 루프백 외부에 노출됩니다 (최소 권한: --bind 127.0.0.1)");
    }
}

// ---------------------------------------------------------------------------
// CLI-facing convenience — shared by `hesmos serve` and the devserve binary
// ---------------------------------------------------------------------------

/// Parse the CLI-6 flag surface `--bind <addr>` / `--port <n>` (defaults
/// 127.0.0.1:7330). Hand-rolled on purpose: two flags don't justify a clap
/// dependency inside the gateway, and both callers need identical semantics.
/// `Err` carries a Korean usage message → callers print it with their own
/// USAGE exit path.
///
/// # Errors
/// Unknown flag, missing value, or a value that fails to parse as an IP/port.
pub fn parse_serve_flags(args: impl Iterator<Item = String>) -> Result<(IpAddr, u16), String> {
    let mut bind: Option<IpAddr> = None;
    let mut port: Option<u16> = None;
    let mut it = args;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--bind" => {
                bind = Some(
                    it.next()
                        .ok_or("--bind 값이 없습니다")?
                        .parse()
                        .map_err(|_| "--bind 값이 IP 주소가 아닙니다")?,
                )
            }
            "--port" => {
                port = Some(
                    it.next()
                        .ok_or("--port 값이 없습니다")?
                        .parse()
                        .map_err(|_| "--port 값이 0~65535 정수가 아닙니다")?,
                )
            }
            other => return Err(format!("알 수 없는 인자 '{other}'")),
        }
    }
    Ok((
        bind.unwrap_or(IpAddr::from([127, 0, 0, 1])),
        port.unwrap_or(7330),
    ))
}

/// Blocking entry point for `hesmos serve` (CLI-6): builds a private runtime,
/// binds, emits the §9.1 banner (+ external-bind warning), then serves until
/// SIGINT/SIGTERM. The CLI keeps no async runtime of its own, so this is the
/// one synchronous call it makes. `Err` (bind failure e.g. port taken, or a
/// serve error) maps to exit 3 on the caller side.
///
/// # Errors
/// Propagates [`bind`] and serve-loop failures.
pub fn run_blocking(bind_addr: IpAddr, port: u16, core: Arc<dyn CorePort>) -> io::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let running = bind(bind_addr, port, core).await?;
            warn_external_bind(bind_addr);
            print_banner(&running.local_url());
            running.serve_until_shutdown().await
        })
}
