# auwsx — Feature Plan

Backlog only. One line per feature. No checklists.

## IN PROGRESS

(none)

## TODO

### main-jobs-queue
Per-project main-branch serializer so routines never race the pipeline (one main-branch writer at a time).

### routines
Second execution lane (report/idea/knowledge); auwsx owns commit + `writable_paths` scope check; built-ins triage/deepsleep/dream/morning-summary.

### inbox-watcher
`notify` watcher on `~/.auwsx/inbox/*.txt` → backlog_items (source=inbox, auto-approved).

### config-load
Load/save project + daemon config (agent cmds, timeouts, policies, skill_path).

### agent-token-scoping
Per-run agent token + caller scoping over IPC (agent vs human op subset); v1 boundary is the 0700 socket.

### launchd
launchd install/uninstall for the daemon (`launchd.rs` stub).

### tui-v0.1
ratatui front-end: views {Overview, Issue, Backlog, Routines, Logs, Config}; keybinds bound to the IPC CRUD matrix.
