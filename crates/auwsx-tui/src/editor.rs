//! $EDITOR shell-out helper.
//!
//! Used for: drafts (n), task body direct (N), followups (f), feedback (e),
//! routine prompt (n in Main view), custom one-off prompt (p).
//!
//! Pattern matches `git commit -e`: spawn $EDITOR on a pre-seeded tmpfile in
//! the project dir (NEVER `/tmp` per project policy), read back on close,
//! delete tmpfile.
//!
//! Empty save is meaningful for feedback flow: it means "no feedback" (the
//! task stays PENDING_FEEDBACK, no auto-advance from followups).

use anyhow::Result;
use std::path::PathBuf;

pub struct EditorResult {
    pub body: String,
    pub was_empty: bool,
}

pub async fn open_editor(
    _seed: &str,
    _tmpfile_dir: &std::path::Path,
    _filename_hint: &str,
) -> Result<EditorResult> {
    // TODO: write seed to <tmpfile_dir>/<filename_hint>.<rand>.md
    // TODO: spawn $EDITOR (fallback nvim/vim/nano/vi); wait
    // TODO: read back, return EditorResult { body, was_empty }
    // TODO: cleanup tmpfile
    todo!("open_editor")
}
