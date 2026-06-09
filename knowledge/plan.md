# auwsx — Feature Plan

## IN PROGRESS

### agent-runner-claude
AgentRunner trait + ClaudeRunner — first real wsx-core consumer; spawns Claude headless in a per-task tmux session, captures log, signals done/timeout. Unblocks pipeline ITERATING/QA.

## TODO

### pipeline-prepare
`pipeline::prepare(task)` — create worktree, copy env, run post-create hook, transition QUEUED→PREPARING→ITERATING.

### scheduler-ticker
Per-project tokio ticker driving tasks + routines.

### tui-v0.1
ratatui front-end: project/task/routine lists, artifact pane, config view.
