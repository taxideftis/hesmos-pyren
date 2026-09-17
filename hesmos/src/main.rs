//! hesmos — the Agent Hesmos CLI (CLI-1..6).
//!
//! Parses and dispatches — nothing else (code-structure §3: no state, no duplicated
//! core logic). The command bodies live in the LIBRARY (`hesmos::cmd`) so the E2E
//! harness drives the same paths in-process; this binary only maps argv onto those
//! calls and exits with the returned band. The `serve` arm exists only with the
//! feature (code-structure §4 — the default build never references the gateway stack).

use clap::{Parser, Subcommand};

use hesmos::cmd;

#[derive(Parser)]
#[command(
    name = "hesmos",
    version,
    about = "Agent Hesmos — deterministic multi-agent core CLI",
    after_help = EXIT_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a plan (CLI-1).
    Run {
        /// Plan YAML path (TYPE-6 raw surface).
        plan: std::path::PathBuf,
        /// u64 seed; omitted = OS entropy (the recorded value shows in the banner).
        #[arg(long)]
        seed: Option<u64>,
        /// Budget envelope, frozen at start: `tokens=N` | `tokens=unbounded`.
        #[arg(long = "budget", value_name = "KEY=VAL")]
        budget: Option<String>,
        /// Organization marker (data only, no RBAC — charter §5).
        #[arg(long = "team", value_name = "TEAM_ID")]
        team: Option<String>,
        /// Compile + wave schedule only — no session, no events, no calls.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Inspect a session trace / replay (CLI-2/3).
    Trace {
        #[command(subcommand)]
        command: TraceCommand,
    },
    /// Inspect the metering ledger (CLI-4).
    Budget {
        /// Session id — omit when using --team.
        session_id: Option<String>,
        /// Team-scope aggregation across sessions.
        #[arg(long = "team", value_name = "TEAM_ID")]
        team: Option<String>,
        /// Machine-readable output.
        #[arg(long = "json")]
        json: bool,
    },
    /// Run a golden eval suite (CLI-5; lands in WP-P3a).
    Eval,
    /// Serve the read-only dashboard (CLI-6; requires `--features serve`).
    #[cfg(feature = "serve")]
    Serve,
}

#[derive(Subcommand)]
enum TraceCommand {
    /// Render the session timeline — full chain verification FIRST (CLI-2).
    Show {
        session_id: String,
        /// Only gate.pass/gate.fail rows (no folding).
        #[arg(long = "gate")]
        gate: bool,
        /// Only handoff.request/handoff.accept rows (no folding).
        #[arg(long = "handoff")]
        handoff: bool,
        /// Physical row cap (folding aside) — overflow prints the extension hint.
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Raw event stream — one JSON per line, no fold, no color.
        #[arg(long = "json")]
        json: bool,
    },
    /// Fork-replay at a commit point — NEW session, fork_of lineage (CLI-3).
    Replay {
        session_id: String,
        /// Commit point: `--at step N` (N = CommitSeq from trace show) or `--at N`.
        /// Omit to replay the whole session from the start.
        #[arg(long = "at", value_name = "STEP N", num_args = 1..=2)]
        at: Vec<String>,
        /// Re-pin the fork's frozen envelope: `tokens=N` | `tokens=unbounded`.
        #[arg(long = "budget", value_name = "KEY=VAL")]
        budget: Option<String>,
    },
}

const EXIT_HELP: &str = "exit codes: 0 ok · 2 usage · 3 compile/세션 없음 · 10 budget suspended · 11 halted(loop) · 12 halted(provider) · 20 failed(gate reject) · 30 evidence invalid · 130 SIGINT";

fn main() {
    let cli = Cli::parse();
    let root = std::env::current_dir().expect("cwd resolves");
    let code = match cli.command {
        Command::Run {
            plan,
            seed,
            budget,
            team,
            dry_run,
        } => cmd::run::execute(
            cmd::run::RunArgs {
                plan,
                seed,
                budget,
                team,
                dry_run,
            },
            &root,
        ),
        Command::Trace {
            command:
                TraceCommand::Show {
                    session_id,
                    gate,
                    handoff,
                    limit,
                    json,
                },
        } => cmd::trace::show(
            cmd::trace::ShowArgs {
                session_id,
                gate,
                handoff,
                limit,
                json,
            },
            &root,
        ),
        Command::Trace {
            command:
                TraceCommand::Replay {
                    session_id,
                    at,
                    budget,
                },
        } => match parse_at(&at) {
            Ok(at) => cmd::trace::replay(
                cmd::trace::ReplayArgs {
                    session_id,
                    at,
                    budget,
                },
                &root,
            ),
            Err(()) => cmd::run::usage_error(
                "--at 값이 올바르지 않습니다 — `--at step <N>` 또는 `--at <N>` (N=CommitSeq)"
                    .into(),
            ),
        },
        Command::Budget {
            session_id,
            team,
            json,
        } => cmd::budget::execute(
            cmd::budget::BudgetArgs {
                session_id,
                team,
                json,
            },
            &root,
        ),
        Command::Eval => cmd::eval::dispatch(),
        // Pre-wired seat (code-structure §4): implementation is Andrew's (WP-P3c).
        #[cfg(feature = "serve")]
        Command::Serve => cmd::serve::dispatch(),
    };
    std::process::exit(code);
}

/// `--at` accepts the contract spelling `step N` (two tokens) or a bare `N`. Anything
/// else is a usage error — it must NOT silently degrade into a whole-session replay.
fn parse_at(raw: &[String]) -> Result<Option<u64>, ()> {
    match raw {
        [] => Ok(None),
        [one] => one.parse::<u64>().map(Some).map_err(|_| ()),
        [word, n] if word == "step" => n.parse::<u64>().map(Some).map_err(|_| ()),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W3-3: the documented spelling is `--at step N`; the bare number stays accepted
    /// for scripts. A malformed pair must NOT silently become a commit point.
    #[test]
    fn at_flag_parses_both_spellings() {
        assert_eq!(parse_at(&[]), Ok(None));
        assert_eq!(parse_at(&["7".to_string()]), Ok(Some(7)));
        assert_eq!(
            parse_at(&["step".to_string(), "3".to_string()]),
            Ok(Some(3))
        );
        assert_eq!(parse_at(&["walk".to_string(), "3".to_string()]), Err(()));
        assert_eq!(
            parse_at(&["x".to_string(), "y".to_string(), "z".to_string()]),
            Err(())
        );
        assert_eq!(parse_at(&["step".to_string(), "x".to_string()]), Err(()));
    }
}
