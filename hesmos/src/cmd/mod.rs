//! Command modules (one file per CLI contract). P0a ships dispatch skeletons only —
//! every command reports not-implemented with exit code 2 until its WP lands
//! (run/trace/budget/eval → WP-P1e, serve → Andrew WP-P3c).

pub mod budget;
pub mod eval;
pub mod run;
#[cfg(feature = "serve")]
pub mod serve;
pub mod trace;

/// The one not-implemented message path, so skeleton commands stay uniform. Kept in
/// this module (not a separate catalog file) until WP-P1e introduces the real message
/// catalog — one file, one format, no per-command ad-hoc strings.
pub(crate) fn not_implemented(cmd: &str) -> i32 {
    eprintln!("hesmos {cmd}: not implemented in the P0a skeleton (lands in a later WP)");
    crate::exit::EXIT_USAGE
}
