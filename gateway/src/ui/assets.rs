//! Static asset embedding — `include_str!` keeps the "one CSS + one JS"
//! constraint literal: no asset pipeline, no separate static dir to deploy.

pub const CSS: &str = include_str!("assets/hesmos.css");
pub const JS: &str = include_str!("assets/hesmos.js");
