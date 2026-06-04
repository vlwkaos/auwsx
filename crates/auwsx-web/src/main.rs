//! auwsx-web — v0.2 ship target. Thin axum translator over the daemon IPC socket.
//!
//! Status: DEFERRED. Stub only. See plan Step 7b / Step 8.
//!
//! When implemented:
//!   - Connect to `$XDG_RUNTIME_DIR/auwsx.sock`.
//!   - Expose REST endpoints listed in plan Step 7b.
//!   - Stream Events as SSE on `/api/events`.
//!   - Serve embedded React bundle from `web/dist/` via rust-embed.

fn main() {
    eprintln!("auwsx-web is a v0.2 placeholder. Use the TUI (`auwsx`) for v0.1.");
}
