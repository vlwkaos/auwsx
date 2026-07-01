# auwsx-web

This directory will hold the React + Vite + Tailwind app that consumes
`auwsx-web` (axum) over REST + SSE. The Rust `auwsx-web` binary now exists as a
thin HTTP adapter over daemon IPC for remote repository webhooks.

**Frontend status: not started.** v0.1 is still the ratatui TUI.

## Webhook Adapter

Run the daemon first, then start the web adapter:

```bash
cargo run --bin auwsx -- daemon
cargo run --bin auwsx-web
```

`auwsx-web` listens on `AUWSX_WEB_ADDR` or `127.0.0.1:7789` by default and
connects to the daemon at the normal `AUWSX_SOCK` path.

GitHub webhook endpoint:

```text
POST /webhooks/github
```

Behavior:

- Parses GitHub `issue_comment` payloads and normalizes them to daemon IPC
  `ProcessRemoteAuwsxRun`.
- Resolves the per-project remote config by `provider/owner/repo`.
- If `webhook_secret_ref` is set, it is treated as an environment variable name
  or `env:NAME`, and `X-Hub-Signature-256` must match.
- Durable mutation remains daemon-owned: accepted `/auwsx-run` comments become
  approved inbox backlog through `auwsx-core::remote_inbound`.

See the design plan at `~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md`,
Step 8 (frontend layout) and the universal CRUD matrix (Step 3.9). Each row in
the matrix maps to exactly one widget here.

Planned components:
- `src/pages/Workspace.tsx` — three-pane shell
- `src/components/{ProjectList,TaskList,ArtifactPanel,FeedbackBox,RoutinesPanel}.tsx`
