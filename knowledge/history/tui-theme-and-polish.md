---
slug: tui-theme-and-polish
kind: history
branch: main
completed: 2026-06-17
---

# TUI Theme and Polish

## Summary

The TUI gained a real multi-project tree, centralized theme roles, version
footer, dependency-free git repository search, scheduling controls/countdowns,
and quit-with-daemon-stop flow.

## Key Decisions

- Tree rows carry `project_id` so actions target the row's project.
- Theme colors are semantic roles only; inline `Color::` is banned under UI
  modules except `theme.rs`.
- Repository search avoids new runtime dependencies and requires `.git` to be a
  directory.
- `schedule_interval_min=0` means evaluate every daemon loop, not spawn
  constantly.

## Knowledge Created/Updated

- `coding/tui-tree-theme-repo-search.md` - reusable tree/theme/repo-search rules.
- `domain/project-config-and-scheduling.md` - scheduler cadence and config knobs.
- `coding/scheduler-pipeline.md` - auto-tick cadence.
- `coding/cli-parse-grammar.md` - `--schedule` project flag.
- `coding/good-to-go-axes.md` - test-target coverage rule after shape changes.

## Implementation Notes

Work progressed from tree/theme/footer/repo search into scheduling UI and daemon
config plumbing. Verification converged on full package tests rather than build
only because test-only constructors caught API shape drift.
