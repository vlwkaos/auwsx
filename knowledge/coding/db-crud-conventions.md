---
slug: db-crud-conventions
kind: coding
title: auwsx typed DB CRUD conventions
description: Conventions for the typed SQLite CRUD layer — injected `now` clock, hand-rolled as_str/from_str vs serde split, per-struct from_row, transition vs force_status, run_triage v1, and mutators returning Err on missing id.
keywords: [injected now clock epoch ms, as_str from_str hand-rolled, serde snake_case JSON path, from_row try_get, no FromRow derive, transition force_status mark_absorbed, steering add transactional has_pending_steering, run_triage no grouping, mutators Err on missing id, rows_affected, agent_runs start finish, ask_answers append-only, issue_id main_job_id xor, create override coalesce schedule_cron, typed crud]
created: 2026-06-09
modified: 2026-06-17
---

# auwsx typed DB CRUD conventions

Typed CRUD lives in `db/{projects,issues,subtasks,findings,agent_runs}.rs` plus
CRUD in `backlog.rs`/`steering.rs`. Row structs + async CRUD fns.

## Clock injected

Every stamping CRUD fn takes `now: i64` (epoch ms). Keeps CRUD
deterministic/unit-testable; the daemon (`ipc::now_ms`) is the single clock owner
and supplies real time per request. **Do NOT call `SystemTime` inside CRUD.**

## Enum ↔ SQL two-path split

- **SQL bind path**: hand-rolled `as_str`/`from_str` (mirroring `IssueStatus`),
  matching the SQL CHECK domain exactly.
- **JSON/IPC path**: `#[serde(rename_all="snake_case")]`.
- Both must agree (they do). Enum pairs: `IssueStatus`, `MainJobStatus`,
  `backlog::Source`/`Approval`, `steering::SteeringSource`, `MergeMode`,
  `CompletionPolicy`, `Severity`, `FindingStatus`, `agent_runs::Role`,
  `agent::ExitKind`. Parity table in coding/good-to-go-axes.md.

## Row decode

Per-struct `from_row(&SqliteRow) -> Result<Self>`: `try_get` each column, parse
enum strings with `from_str().ok_or_else(..)`. **No `FromRow` derive** — enum
columns need custom parse.

## Mutators

- Return **Err on missing id** (`rows_affected == 0` → not-found) so callers can
  distinguish. Shared `ensure_found(rows_affected, id)` helper. (A generic HRTB
  `update_one` closure was tried and abandoned — sqlx `'q` lifetime fought the
  `for<'q>` bound; inline each UPDATE instead.)

## Key operations

| Fn | Behavior |
|----|----------|
| `issues::transition()` | enforces `state::check_transition` |
| `issues::force_status()` | human-override bypass (caller logs it) |
| `issues::mark_absorbed` | CONSOLIDATING→ABSORBED + target id |
| `steering::add` | transactional: append note + set `has_pending_steering=1`; guarded by `IssueStatus::accepts_steering` (working phases only) |
| `backlog::run_triage` | v1, no grouping: promote each Approved + ungrouped item into its own CONSOLIDATING issue, set `consumed_issue_id` |
| `agent_runs` | append-only, two-step `start` (spawn) / `finish` (exit); `issue_id`/`main_job_id` XOR enforced in code |
| `ask_answers` | append-only project Q&A history; newest-first reads; `AskMode` follows SQL enum parity |

## create-override-coalesce-default

Exposing a subset of policy columns on a `create` WITHOUT duplicating SQL
DEFAULTs in Rust. **The migration stays the single source of default values.**
(Applied to `projects::create`; columns themselves are in domain/db-schema.md.)

- `NewProject` gains `Option` override fields — `completion_policy:
  Option<CompletionPolicy>`, `plan_gate_timeout_min: Option<i64>`,
  `completion_soft_timeout_min: Option<i64>`, `schedule_cron: Option<&str>`.
  `None` ⇒ keep DB DEFAULT; `Some` ⇒ override only that column.
- `create` does the base INSERT (unchanged columns take DEFAULT via RETURNING
  id), then **conditionally** runs ONE UPDATE only when ≥1 override is `Some`:
  ```sql
  UPDATE projects SET
    completion_policy = COALESCE(?, completion_policy),
    plan_gate_timeout_min = COALESCE(?, plan_gate_timeout_min),
    completion_soft_timeout_min = COALESCE(?, completion_soft_timeout_min),
    schedule_cron = COALESCE(?, schedule_cron)
  WHERE id = ?
  ```
  Bind `completion_policy.map(|p| p.as_str())` and `schedule_cron` as
  `Option<&str>`; integer overrides bind directly. `COALESCE(NULL, col)` keeps
  the just-defaulted value ⇒ siblings
  untouched.
- Guard `if a.is_some() || b.is_some() || c.is_some()` skips the no-op write
  when all `None` (defaults persist on the INSERT alone).
- **Why not bind defaults from Rust**: would duplicate the migration's DEFAULT
  in two places. **Why not dynamic SQL**: COALESCE is one static statement.
- CRUD-layer cousin of the schema-level `review_agent_cmd NULL → work_agent_cmd`
  fallback in domain/db-schema.md.

## Note

`db/mod.rs` had NO typed CRUD originally; insert helpers lived in test files — so
the 2026-06-09 schema rewrite only broke `tests/db_smoke.rs` (regenerated), not
the lib. The typed CRUD layer was then added on top.
