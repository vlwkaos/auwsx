//! auwsx TUI binary entry point. Plan Step 7.
//!
//! Subcommands:
//!   auwsx                              → tui (default)
//!   auwsx tui [--focus task:{id}]      → ratatui front-end
//!   auwsx daemon                       → run scheduler/pipeline/IPC server in foreground
//!   auwsx daemon stop                  → graceful shutdown via IPC
//!   auwsx daemon install-launchd       → write LaunchAgent plist
//!   auwsx daemon uninstall-launchd     → remove LaunchAgent
//!
//! TUI auto-starts the daemon (forks + waits for socket) if none is running.

mod app;
mod cli;
mod editor;
mod input;
mod ui;

use anyhow::Result;
use cli::CliAction;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args)? {
        CliAction::Daemon => cli::run_daemon().await,
        CliAction::Request(cmd) => cli::run_request(cmd).await,
        CliAction::Help => {
            cli::print_usage();
            Ok(())
        }
        // TODO(tui slice): ensure inside tmux, auto-start daemon, run the
        // ratatui loop subscribed to the IPC Event stream. Until then, point the
        // user at the working CLI surface.
        CliAction::Tui => {
            eprintln!("auwsx: TUI not yet implemented — run `auwsx help` for the CLI.");
            Ok(())
        }
    }
}
