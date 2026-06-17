# auwsx — Feature Plan

Backlog only. One line per feature. No checklists.

## TODO

### main-jobs-queue
Per-project main-branch serializer so routines never race the pipeline (one main-branch writer at a time).

### routines
Second execution lane (report/idea/knowledge); auwsx owns commit + `writable_paths` scope check; built-ins triage/deepsleep/dream/morning-summary.

### inbox-watcher
`notify` watcher on `~/.auwsx/inbox/*.txt` → backlog_items (source=inbox, auto-approved).

### config-load
Load/save project + daemon config (agent cmds, timeouts, policies, schedule interval, skill_path).

### agent-token-scoping
Per-run agent token + caller scoping over IPC (agent vs human op subset); v1 boundary is the 0700 socket.

### launchd
launchd install/uninstall for the daemon (`launchd.rs` stub).

### project-update-command
Supported IPC/CLI path to edit existing project config fields without direct SQLite edits.

### tui-mock-daemon-integration-tests
Integration tests for TUI async apply/refresh/event-stream behavior against a mock daemon.

### tui-config-field-hints
Per-field config hints in the TUI project form for terse scheduler/gate labels.

### board-backlog-ordering
Make TODO-lane backlog ordering explicit instead of relying on daemon-return order.

### shared-log-prettifier
Extract the agent log prettifier from TUI rendering if web or CLI surfaces need the same formatting.

### clippy-warning-debt
Resolve existing clippy warning debt separately from feature work.
