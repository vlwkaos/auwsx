//! File-watch inbox channel. Plan Step 3.65 / north star §3.
//!
//! Watches `~/.auwsx/inbox/{project_name}.txt` per project via `notify` crate.
//! Per-line semantics:
//!
//! ```text
//! # comment           → ignored
//! > body              → followup against "the currently iterating task"
//!                       (must be exactly one to disambiguate; else → failed.log)
//! >123: body          → followup against task id 123
//! body                → draft
//! ```
//!
//! After ingest: move consumed lines to `{project}.consumed.log` (append),
//! failed lines to `{project}.failed.log` (append, with reason), truncate live file.
//!
//! Designed for async drop-in: append from vim/ssh/Termius/Obsidian, walk away.

// TODO: spawn_watcher(project_id, project_name, inbox_dir) -> JoinHandle
// TODO: parse_line(line) -> InboxLine { Comment | Draft | FollowupForCurrent | FollowupForId(id) }
// TODO: dispatch + log writers
