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
mod ui;

use anyhow::Result;
use auwsx_core::ipc::{self, Command};
use cli::CliAction;
use std::process::Stdio;
use std::time::Duration;

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
        // ratatui dashboard over the IPC client; starts the daemon when absent.
        CliAction::Tui => {
            let socket = ipc::default_socket_path();
            ensure_daemon(&socket).await?;
            app::run(socket).await
        }
    }
}

async fn ensure_daemon(socket: &std::path::Path) -> Result<()> {
    if ipc::request(socket, &Command::Ping).await.is_ok() {
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut last_err = None;
    for _ in 0..50 {
        match ipc::request(socket, &Command::Ping).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    match last_err {
        Some(e) => Err(e).map_err(Into::into),
        None => anyhow::bail!("daemon did not become ready"),
    }
}
