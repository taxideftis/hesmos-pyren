//! Agent Hesmos gateway — read-only HTTP/WS adapter over the core (SS-24).
//!
//! Layering contract (project-context §8, SS-24 rules 1–2):
//! - Routes and UI hold **no session state**: every request maps onto a
//!   [`core_port::CorePort`] query. The gateway never calls bathos directly and
//!   never recomputes audit chains — verify status arrives as a pass-through
//!   bool asked for by the core (HTTP-5 sourcing rule, P8).
//! - All wire shapes come from `api-contracts.md §7` (HTTP-1~5); responses
//!   carry exactly the contract fields, no invented columns.
//! - The dashboard (D1–D4) is read-only: GET forms and client-side read aids
//!   only — no endpoint may mutate core state (ui-spec §9.0).
//!
//! `stub` is a temporary in-process stand-in for the core while
//! `hesmos-core` (Phillip, WP-P0a/P1e) is not yet on the workspace. It lives
//! strictly behind [`core_port::CorePort`], so routes/UI never know it exists;
//! swapping in the real core adapter must not touch anything under `routes/`
//! or `ui/`.
pub mod core_port;
pub mod error;
pub mod routes;
pub mod serve;
pub mod stream;
pub mod stub;
pub mod ui;
