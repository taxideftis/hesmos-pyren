//! hesmos — the Agent Hesmos CLI (CLI-1..6).
//!
//! Parses, dispatches, renders and maps exit codes — nothing else (code-structure §3:
//! no state, no duplicated core logic). Command implementations land in WP-P1e; the
//! P0a skeleton reserves the dispatch table and the `serve` feature seat.

mod cmd;
mod exit;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "hesmos",
    version,
    about = "Agent Hesmos — deterministic multi-agent core CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a plan (CLI-1).
    Run,
    /// Inspect a session trace / replay (CLI-2/3).
    Trace,
    /// Inspect the metering ledger (CLI-4).
    Budget,
    /// Run a golden eval suite (CLI-5).
    Eval,
    /// Serve the read-only dashboard (CLI-6; requires `--features serve`).
    #[cfg(feature = "serve")]
    Serve,
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Run => cmd::run::dispatch(),
        Command::Trace => cmd::trace::dispatch(),
        Command::Budget => cmd::budget::dispatch(),
        Command::Eval => cmd::eval::dispatch(),
        // Pre-wired seat (code-structure §4): the arm exists only with the feature, so
        // the default build never references the gateway stack.
        #[cfg(feature = "serve")]
        Command::Serve => cmd::serve::dispatch(),
    };
    std::process::exit(code);
}
