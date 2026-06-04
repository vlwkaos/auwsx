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
mod editor;
mod input;
mod ui;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // TODO: clap-less arg parse (we want to stay light); dispatch to subcommand.
    // TODO: ensure inside tmux (same constraint wsx has) — error out otherwise.
    // TODO: connect to daemon socket; auto-start if absent.
    // TODO: run app::App::run() — ratatui loop subscribed to Event stream.
    eprintln!("auwsx scaffold — see plan ~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md");
    Ok(())
}
