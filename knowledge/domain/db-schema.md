---
slug: db-schema
kind: domain
title: auwsx SQLite schema (migrations, 12 tables)
description: The 12-table SQLite schema for the issue model, including projects config, global Arsenal presets, issues, subtasks, findings, steering, backlog_items, ask_answers, routines, main_jobs, agent_runs, and scheduler_runs.
keywords: [0001_init.sql, 0003_arsenal_presets.sql, 0004_ask_answers.sql, SQLite schema, CHECK domain, foreign_keys per connection, agent_runs append-only, steering append-only, ask_answers append-only, backlog source approval gate, arsenal presets, projects config columns, schedule_interval_min, issues columns, findings severity lens, routines writable_paths, main_jobs REJECTED scope_violation, scheduler_runs, completion_policy, merge_mode, review_max_rounds, conflict_max_attempts, db migration]
created: 2026-06-09
modified: 2026-06-17
---

# auwsx db schema

`crates/auwsx-core/src/db/migrations/` — 12 tables, all enum-like
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
| **arsenal_agent_presets** | global agent command presets | reusable `main/plan/work/review_agent_cmd` templates; seeded built-ins: `codex`, `claude`; project rows store resolved command strings |
| **projects** | per-project config | per-role agent cmds `main/plan/work/review_agent_cmd` (review NULL → fall back to work); `schedule_interval_min` (NULL manual, `<=0` every daemon tick, positive minutes); `completion_policy` (manual\|soft\|auto), `completion_soft_timeout_min`, `plan_gate_timeout_min`, `iteration_timeout_min`, `main_job_timeout_min`, `review_max_rounds`, `conflict_max_attempts`, `max_concurrency`, `merge_mode` (local\|pr), `skill_path`, deepsleep fields |
| **issues** | pipeline unit | `status` (16-state CHECK), `branch`/`worktree_path`/`agent_session` (set at PLANNING), `review_round`, `conflict_attempts`, `wait_until`, `absorbed_into_id`, `has_pending_steering` |
| **subtasks** | issue checklist | A add; A/H check; H edit/rm |
| **findings** | review output | `severity` (blocker\|major\|minor\|nit), `lens`, `status` (open\|accepted\|rejected\|dismissed), adjudication |
| **steering** | append-only guidance | `source` (human\|consolidation); append-only; guarded by `accepts_steering` (working phases only) |
| **backlog_items** | intake | `source` (human\|agent\|routine\|inbox) + `approval` (pending\|approved\|dismissed); **only `approved` flows to triage**; `consumed_issue_id` links promoted history while live lists hide consumed rows |
| **ask_answers** | operator Q&A history | project-level ask-mode stack; `mode` recall\|seek, question, answer, context summary, log path, newest-first by `created_at` |
| **routines** | 2nd execution lane | `type` (report\|idea\|knowledge); `writable_paths` JSON |
| **main_jobs** | main-branch run records | `status` (+REJECTED); `report_path`; `scope_violation` |
| **agent_runs** | append-only action log | `role`/`phase`/`status_before`/`status_after`/`exit_kind`/`log_path`/`spawned_at`/`exited_at`; `issue_id` XOR `main_job_id` (enforced in code) |
| **scheduler_runs** | scheduler tick log | — |

## Append-only / gate rules

- **agent_runs**, **steering**, and **ask_answers** are append-only.
- **backlog admission gate**: human/inbox → `approved` auto; agent/routine →
  `pending`. Only `approved` items are promoted by triage; once promoted, the
  row is retained by id but no longer appears in normal backlog lists.
- **issue removal**: issue child rows cascade. Backlog links are
  `ON DELETE SET NULL`, so the daemon marks linked backlog rows dismissed before
  deleting an issue; removed promoted work must not reappear as live backlog.
- **plan artifact**: immutable post-set; written only by the plan agent;
  human approve/reject at the PLANNED gate.

## CHECK domain parity

Every Rust enum that serializes into a CHECK-constrained TEXT column must match
that domain exactly, both directions. The enums use hand-rolled `as_str`/`from_str`
(NOT serde) for the SQL bind. `tests/crud.rs` proves parity at runtime. Full
parity table + the drift-check `rg` recipe live in coding/good-to-go-axes.md.
