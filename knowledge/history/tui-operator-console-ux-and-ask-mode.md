---
slug: tui-operator-console-ux-and-ask-mode
kind: history
branch: main
completed: 2026-06-17
---

# TUI Operator Console UX and Ask Mode

## Summary

The operator console gained global Arsenal presets, project ask mode, issue
removal, broad issue-board lanes, context-aware hints, readable agent logs, and
more compact issue panes.

## Key Decisions

- Ask mode is daemon-owned, persists newest-first answers, and emits an answer
  event without mutating issue status.
- The board groups detailed `IssueStatus` values into four broad lanes while
  keeping detailed status as the scheduler source of truth.
- Issue and overview logs share one renderer so Codex JSONL and pasted content
  remain readable in both views.

## Knowledge Created/Updated

- `domain/db-schema.md` - Arsenal and ask-answer schema context.
- `domain/issue-model.md` - board lane mapping and ordering.
- `coding/ipc-protocol.md` - AskProject/ListAskAnswers/AskAnswered behavior.
- `coding/agent-subprocess-runner.md` - ask mode execution path.
- `coding/agent-log-prettifier.md` - transcript rendering rules.
- `coding/issue-pane-layout.md` - compact issue layout rules.

## Implementation Notes

The session finished with package tests, clippy, build, diff-check, and the UI
color grep passing. Existing clippy warning debt remains a separate backlog item.
