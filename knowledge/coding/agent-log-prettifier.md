---
slug: agent-log-prettifier
kind: coding
title: TUI agent log prettifier
description: Shared rendering rules for turning Codex JSONL and shell output into bounded, scrollable TUI transcript blocks.
keywords: [agent log, log_block, Codex JSONL, agent_message, shell wrapper stripping, escaped newline decode, bounded transcript, issue_log_scroll, overview log, Issue view log, readable logs]
created: 2026-06-17
modified: 2026-06-17
---

# TUI agent log prettifier

The full Issue view and overview issue detail should use the same renderer:
`ui::issue::log_block`. Avoid direct `Line::raw(app.log_tail.clone())` in other
views, because that bypasses wrapping and JSONL decoding.

## Rendering contract

| Input | Rendering rule |
|-------|----------------|
| Codex thread/turn/item events | short labels instead of raw JSON |
| command events | strip shell wrappers such as `/bin/zsh -lc '...'` |
| `agent_message` | decode literal `\n`, indent message blocks, hard-wrap long lines |
| long code or pasted messages | cap with an omitted-line count |
| log tail | fetch enough bytes for context, currently 128 KiB |

Scroll state belongs in `App::issue_log_scroll`. Issue view keys: `k` older,
`j` newer, `PgUp/PgDn` page, `Home/End` oldest/newest. Footer hints switch to
log-scroll controls while the Issue view is focused.
