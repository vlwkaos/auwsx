---
slug: project-config-cli-flags
kind: history
title: project-config-cli-flags shipped — completion_policy + gate timeouts on `auwsx project add`
description: Record of the feature that threaded three optional policy overrides through NewProject/Command::AddProject/CLI, closing the live autonomous loop to DONE from the CLI alone without raw SQL.
keywords: [project-config-cli-flags shipped, completion_policy flag, plan-gate-timeout, completion-timeout, NewProject overrides, COALESCE single source of defaults, opt_int, autonomous DONE no raw SQL, drive_project, commit ed72491, 448 tests]
created: 2026-06-09
modified: 2026-06-09
---

# project-config-cli-flags (shipped 2026-06-09)

The last knob needed to close the live autonomy loop to `DONE` from the CLI
alone. An autonomous issue now runs `CONSOLIDATING → DONE` driven entirely by
`auwsx project add --completion-policy auto --plan-gate-timeout 0`, replacing the
raw-sqlx `UPDATE projects SET …` workaround the drive-loop test relied on.

**Commit `ed72491`** (on `main`). **448 tests green (+32 added)**, clean
build/clippy, `/good-to-go` + `/backpressure` (2 rounds) passed.

## What shipped

| Surface | Change |
|---------|--------|
| `NewProject` / `Command::AddProject` | 3 optional override fields: `completion_policy: Option<CompletionPolicy>`, `plan_gate_timeout_min: Option<i64>`, `completion_soft_timeout_min: Option<i64>` |
| CLI flags on `project add` | `--completion-policy <manual\|soft\|auto>`, `--plan-gate-timeout <int>`, `--completion-timeout <int>` (→ `completion_soft_timeout_min`) |
| `print_response` Project arm | new `policy:` line (completion / plan_gate / completion_soft / concurrency) so `project get` / `ls` verify the override took |

Patterns documented in coding/db-crud-conventions.md
(create-override-coalesce-default) and coding/cli-parse-grammar.md.

## Key decision — COALESCE single source of defaults

`projects::create` does the base INSERT (DEFAULTs via RETURNING id), then runs
ONE conditional `UPDATE … COALESCE(?, col)` only when ≥1 override is `Some`.
`COALESCE(NULL, col)` keeps the just-defaulted value, so the **migration stays
the single source of default values** — defaults are never duplicated in Rust.

## Impact

- Dropped the raw-sqlx `UPDATE` in `autonomy.rs::drive_project` in favor of the
  typed overrides (`Some(CompletionPolicy::Auto)` + `Some(0)`), simplifying the
  test and removing the last raw-SQL escape hatch in the drive loop.
- 6 fixture call sites (agent/ipc/crud/autonomy) gained `: None` for the new
  fields to preserve prior behavior.
- `/backpressure` Round 1 (`/write-test-audit`) caught 6 contract gaps (i64
  overflow on both timeout flags, repeated-flag last-wins, whitespace policy →
  Err, equals-form timeout, policy-only siblings stay None, soft-timeout-alone
  override). Round 2 closed all 6; loop terminated clean. `/simplify` found no
  source churn warranted.

`config-load` is the natural successor (persist these knobs beyond `project add`).
