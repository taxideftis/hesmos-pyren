//! hesmos library — the CLI contracts' implementations (CLI-1..5) plus the
//! composition-root adapters.
//!
//! The binary (`src/main.rs`) is a thin parse + dispatch + exit shell over this
//! library, so the E2E harness (`tests/integration_rust`) can drive exactly the same
//! code paths in-process. Nothing here duplicates core logic: commands orchestrate the
//! crates and render their outputs; judgment/metering/routing stay in their crates
//! (code-structure §3).

pub mod cmd;
pub mod composition;
pub mod exit;
pub mod messages;
pub mod tokens;
