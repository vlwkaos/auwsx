---
slug: tui-tree-theme-repo-search
kind: coding
title: TUI tree, theme, footer, and repo search conventions
description: Reusable TUI conventions for the multi-project tree model, central theme roles, version footer, and dependency-free git repository search.
keywords: [TUI tree, TreeItem project_id, ProjectChildren cache, expanded projects, ui theme, Color ban, version footer, repo_scan, fuzzy_score, filter_repos, format_repo_path, spawn_blocking scan, repo suggestions]
created: 2026-06-17
modified: 2026-06-17
---

# TUI tree, theme, footer, and repo search

## Multi-project tree

- Every project renders as a depth-0 row. Routines, Backlog, and Issues are
  depth-1 children; concrete items are depth 2.
- `TreeItem` variants carry `project_id`, so actions resolve against the row's
  owning project rather than the globally active project.
- `App` keeps per-project `ProjectChildren` in a cache and an `expanded`
  project-id set. `refresh_all` eager-loads all projects and their children.
- `sync_active_project()` re-derives the active project from the cursor row.
  Enter toggles project expansion or opens issue detail depending on row type.

## Theme rules

All colors under `crates/auwsx-tui/src/ui/` route through `ui::theme`; no inline
`ratatui::style::Color::X` outside `theme.rs`. `BORDER` must stay distinct from
`TEXT_DIM`, `HINT`, and `TEXT` so chrome and content never collapse visually.

Footer rendering splits operator hints on the left and `v{CARGO_PKG_VERSION}` on
the right. Remote update checks are deferred until a real release channel exists.

## Git-repo search

`repo_scan` is dependency-free and intentionally mirrors the wsx pattern:

| Function | Contract |
|----------|----------|
| `scan_git_repos()` | blocking walk from `$HOME`; run from TUI via `spawn_blocking` |
| `walk_for_git` | prunes dotdirs and heavy dirs, stops descending once a repo is found |
| `fuzzy_score` | subsequence score with prefix and consecutive bonuses |
| `filter_repos` | returns top matches; path-like `/` or `~/` query suppresses suggestions |
| `format_repo_path` | uses component-wise `strip_prefix`; never string-prefix trims paths |

`.git` must be a directory for picker purposes. A worktree `.git` file is not
reported as a repository in the TUI picker.
