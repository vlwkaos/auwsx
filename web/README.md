# auwsx-web — v0.2 front-end

This directory will hold the React + Vite + Tailwind app that consumes
`auwsx-web` (axum) over REST + SSE.

**Status: not started.** Ship target v0.2. v0.1 is the ratatui TUI.

See the design plan at `~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md`,
Step 8 (frontend layout) and the universal CRUD matrix (Step 3.9). Each row in
the matrix maps to exactly one widget here.

Planned components:
- `src/pages/Workspace.tsx` — three-pane shell
- `src/components/{ProjectList,TaskList,ArtifactPanel,FeedbackBox,RoutinesPanel}.tsx`
