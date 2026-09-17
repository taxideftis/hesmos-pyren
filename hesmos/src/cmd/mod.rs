//! Command modules (one file per CLI contract; SS-13 rule 1 — no invented commands).
//! run/trace/budget are the WP-P1e implementations; eval is a dispatch skeleton until
//! WP-P3a; serve is Andrew's seat (WP-P3c) behind the `serve` feature.

pub mod budget;
pub mod eval;
pub mod run;
#[cfg(feature = "serve")]
pub mod serve;
pub mod trace;
