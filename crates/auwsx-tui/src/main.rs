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
mod input;
mod repo_scan;
mod ui;

use anyhow::Result;
use auwsx_core::ipc;
use cli::CliAction;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args)? {
        CliAction::Daemon => cli::run_daemon().await,
        CliAction::Request(cmd) => cli::run_request(cmd).await,
        CliAction::PruneWorktrees { repo_path } => cli::run_prune_worktrees(repo_path).await,
        CliAction::Help => {
            cli::print_usage();
            Ok(())
        }
        // ratatui dashboard over the IPC client; starts the daemon when absent.
        CliAction::Tui => {
            let socket = ipc::default_socket_path();
            cli::ensure_daemon(&socket).await?;
            app::run(socket).await
        }
    }
}
