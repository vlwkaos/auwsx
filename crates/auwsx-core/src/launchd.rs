//! launchd integration. Plan Step 7 / north star §1.
//!
//! `auwsx daemon install-launchd` writes `~/Library/LaunchAgents/com.vlwkaos.auwsx.plist`
//! with KeepAlive (auto-restart on crash) + RunAtLoad (start on login).
//!
//! `auwsx daemon uninstall-launchd` unloads + removes the plist.
//!
//! The daemon binary path is determined by `std::env::current_exe()` at install time.

use crate::Result;

pub const LAUNCHD_LABEL: &str = "com.vlwkaos.auwsx";

// TODO: install_launchd() — generate plist, write, `launchctl bootstrap`
// TODO: uninstall_launchd() — `launchctl bootout`, rm plist
// TODO: is_installed() / is_running()
