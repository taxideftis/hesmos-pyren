//! Rust E2E harness crate.
//!
//! Scenario tests live under `tests/` in this crate (not in the lib) so each file is a
//! separate binary and failures stay isolated. P0a lands the member plus the first
//! cross-crate contract smoke tests; WP-P1e adds seed reproduction / loop-guard /
//! budget / seal scenarios (SS-01/08/11/14).
