---
slug: db-schema
kind: domain
title: auwsx SQLite schema (0001_init, 10 tables)
description: The 10-table SQLite schema for the issue model — projects config, issues, subtasks, findings, steering, backlog_items, routines, main_jobs, agent_runs, scheduler_runs — with CHECK domains, per-connection FK enforcement, and append-only/gate rules.
keywords: [0001_init.sql, SQLite schema, CHECK domain, foreign_keys per connection, agent_runs append-only, steering append-only, backlog source approval gate, projects config columns, issues columns, findings severity lens, routines writable_paths, main_jobs REJECTED scope_violation, scheduler_runs, completion_policy, merge_mode, review_max_rounds, conflict_max_attempts, db migration]
created: 2026-06-09
modified: 2026-06-09
---

# auwsx db schema

`crates/auwsx-core/src/db/migrations/0001_init.sql` — 10 tables, all enum-like
TEXT columns constrained by `CHECK (col IN (...))`. The SQL is the source of
truth for every enum domain (see coding/good-to-go-axes.md parity table).

## FK enforcement gotcha

`PRAGMA foreign_keys=ON` **in the migration is a no-op** — sqlx runs migrations
inside a transaction. FK enforcement is set **per-connection** in `db/mod.rs`
(`SqliteConnectOptions::foreign_keys(true)`). The misleading in-migration PRAGMA
was removed.

## Tables

| Table | Role | Notable columns / rules |
|-------|------|-------------------------|
| **projects** | per-project config | per-role agent cmds `main/plan/work/review_agent_cmd` (review NULL → fall back to work); `completion_policy` (manual\|soft\|auto), `completion_soft_timeout_min`, `plan_gate_timeout_min`, `iteration_timeout_min`, `main_job_timeout_min`, `review_max_rounds`, `conflict_max_attempts`, `max_concurrency`, `merge_mode` (local\|pr), `skill_path`, deepsleep fields |
| **issues** | pipeline unit | `status` (16-state CHECK), `branch`/`worktree_path`/`agent_session` (set at PLANNING), `review_round`, `conflict_attempts`, `wait_until`, `absorbed_into_id`, `has_pending_steering` |
| **subtasks** | issue checklist | A add; A/H check; H edit/rm |
| **findings** | review output | `severity` (blocker\|major\|minor\|nit), `lens`, `status` (open\|accepted\|rejected\|dismissed), adjudication |
| **steering** | append-only guidance | `source` (human\|consolidation); append-only; guarded by `accepts_steering` (working phases only) |
| **backlog_items** | intake | `source` (human\|agent\|routine\|inbox) + `approval` (pending\|approved\|dismissed); **only `approved` flows to triage** |
| **routines** | 2nd execution lane | `type` (report\|idea\|knowledge); `writable_paths` JSON |
| **main_jobs** | main-branch run records | `status` (+REJECTED); `report_path`; `scope_violation` |
| **agent_runs** | append-only action log | `role`/`phase`/`status_before`/`status_after`/`exit_kind`/`log_path`/`spawned_at`/`exited_at`; `issue_id` XOR `main_job_id` (enforced in code) |
| **scheduler_runs** | scheduler tick log | — |

## Append-only / gate rules

- **agent_runs** and **steering** are append-only.
- **backlog admission gate**: human/inbox → `approved` auto; agent/routine →
  `pending`. Only `approved` items are promoted by triage.
- **plan artifact**: immutable post-set; written only by the plan agent;
  human approve/reject at the PLANNED gate.

## CHECK domain parity

Every Rust enum that serializes into a CHECK-constrained TEXT column must match
that domain exactly, both directions. The enums use hand-rolled `as_str`/`from_str`
(NOT serde) for the SQL bind. `tests/crud.rs` proves parity at runtime. Full
parity table + the drift-check `rg` recipe live in coding/good-to-go-axes.md.
