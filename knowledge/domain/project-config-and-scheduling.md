---
slug: project-config-and-scheduling
kind: domain
title: Project config and scheduler cadence
description: Durable semantics for project-level scheduling, gate policies, concurrency, and the autonomous-operation configuration recipe.
keywords: [project config, schedule_interval_min, AUWSX_TICK_SECS, completion_policy, plan_gate_timeout_min, completion_soft_timeout_min, iteration_timeout_min, main_job_timeout_min, review_max_rounds, conflict_max_attempts, max_concurrency, merge_mode, autonomous operation, manual tick]
created: 2026-06-17
modified: 2026-06-17
---

# Project config and scheduling

## Schedule cadence

`projects.schedule_interval_min` controls automatic scheduler evaluation:

| Value | Behavior |
|-------|----------|
| NULL | never auto-tick; only explicit scheduler IPC commands run it |
| `<= 0` | due on every global daemon loop |
| `n > 0` | due after `n` minutes since the latest auto tick |

The global loop cadence is `AUWSX_TICK_SECS`, default 10 seconds with a floor of
1 second. A scheduler tick only evaluates state; it spawns an agent only when an
issue is actionable and project concurrency has a free slot.

## Policy groups

| Group | Fields |
|-------|--------|
| Identity | `name`, `repo_path`, `default_branch` |
| Agent commands | `main_agent_cmd`, `plan_agent_cmd`, `work_agent_cmd`, `review_agent_cmd` |
| Scheduling | `schedule_interval_min` |
| Soft gates | `plan_gate_timeout_min`, `completion_policy`, `completion_soft_timeout_min` |
| Hard caps | `iteration_timeout_min`, `main_job_timeout_min`, `review_max_rounds`, `conflict_max_attempts` |
| Integration | `max_concurrency`, `merge_mode` |
| Skills and maintenance | `skill_path`, `deepsleep_interval_days`, `last_deepsleep_at` |

Review command fallback is schema-level: `review_agent_cmd` NULL means use
`work_agent_cmd`. Project creation can use Arsenal presets to fill concrete
command strings, but project rows store the resolved commands.

## Autonomous-operation recipe

For a project that should react quickly and complete without human gates:

| Field | Value |
|-------|-------|
| `schedule_interval_min` | `0` |
| `completion_policy` | `auto` |
| `plan_gate_timeout_min` | `0` |
| backlog admission | approved items only |

Anything else can park work at manual boundaries: NULL scheduling never
auto-runs, `manual` completion waits at ENDED, and positive plan gates delay
PLANNED before implementation.
