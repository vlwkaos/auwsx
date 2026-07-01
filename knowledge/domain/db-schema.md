---
slug: db-schema
kind: domain
title: auwsx SQLite schema (migrations, tables, global settings)
description: The SQLite schema for the issue model, including project config, remote repo config/link/event state, profiles, global settings, Arsenal presets, issues, queue/steering messages, backlog, routines, main_jobs, agent_runs, and scheduler runs.
keywords: [SQLite schema, CHECK domain, foreign_keys per connection, remote repository config, project_remote_configs, remote_issue_links, remote_pr_links, remote_events, remote_sync_runs, global_settings, profiles, arsenal presets, prompt policy, agent_runs append-only, queue messages, steering append-only, ask_answers append-only, backlog source approval gate, projects config columns, schedule_interval_min, issues columns, findings severity lens, routine output route, routines writable_paths, main_jobs REJECTED scope_violation, scheduler_runs, completion_policy, merge_mode, review_max_rounds, conflict_max_attempts, db migration]
created: 2026-06-09
modified: 2026-07-01
---

# auwsx db schema

`crates/auwsx-core/src/db/migrations/` defines the schema. Enum-like TEXT
columns are constrained by `CHECK (col IN (...))`; the SQL is the source of
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
| **global_settings** | singleton operator defaults | persisted prompt/pipeline guidance and other global settings; editable values need IPC read/write coverage, length bounds, and CLI escaping |
| **profiles** | project grouping | profile name/order; projects store profile membership and order within profile |
| **projects** | per-project config | per-role agent cmds `main/plan/work/review_agent_cmd` (review NULL → fall back to work); `schedule_interval_min` (NULL manual, `<=0` every daemon tick, positive minutes); `completion_policy` (manual\|soft\|auto), `completion_soft_timeout_min`, `plan_gate_timeout_min`, `iteration_timeout_min`, `main_job_timeout_min`, `review_max_rounds`, `conflict_max_attempts`, `max_concurrency`, `merge_mode` (local\|pr), `skill_path`, deepsleep fields |
| **project_remote_configs** | per-project remote repo settings | one row per project; provider `github`; repo identity `remote_url`/`owner`/`repo`/`api_base_url`; auth ref/secret ref only (no raw token); toggles for inbound `/auwsx-run`, outbound issue creation, PR merge, agent/subtask/finding comment sync, draft PR, required checks policy |
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
| **remote_issue_links** | local↔remote issue map | links one local issue or backlog item to one provider/owner/repo issue number and URL; unique by remote issue, local issue, and backlog item |
| **remote_pr_links** | local issue↔remote PR map | links one local issue to a remote PR, head/base branches and SHAs, and PR state `open\|closed\|merged` |
| **remote_events** | webhook idempotency log | provider delivery id, event/action, payload hash, status `received\|processed\|ignored\|failed`; unique by provider+delivery id |
| **remote_sync_runs** | remote sync queue/audit log | inbound/outbound sync attempts for webhook/issue/comment/PR operations; status `queued\|running\|done\|failed\|skipped`; `remote_workflow` avoids duplicate active runs per issue/kind |

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

Target rename from 2026-06-18: expose `steering` as queue/issue messages in
operator-facing APIs and UI. Do not model joined backlog as issue status; keep
the join as backlog history plus an append-only message on the target issue.

## CHECK domain parity

Every Rust enum that serializes into a CHECK-constrained TEXT column must match
that domain exactly, both directions. The enums use hand-rolled `as_str`/`from_str`
(NOT serde) for the SQL bind. `tests/crud.rs` proves parity at runtime. Full
parity table + the drift-check `rg` recipe live in coding/good-to-go-axes.md.
