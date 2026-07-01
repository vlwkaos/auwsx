---
slug: project-config-and-scheduling
kind: domain
title: Project config and scheduler cadence
description: Durable semantics for project-level scheduling, gate policies, concurrency, profile assignment, routine output routing, and autonomous-operation configuration.
keywords: [project config, schedule_cron, deepsleep_cron, AUWSX_TICK_SECS, completion_policy, plan_gate_timeout_min, completion_soft_timeout_min, iteration_timeout_min, main_job_timeout_min, review_max_rounds, conflict_max_attempts, max_concurrency, merge_mode, profile, global settings, routine output route, autonomous operation, manual tick]
created: 2026-06-17
modified: 2026-06-24
---

# Project config and scheduling

## Schedule cadence

`projects.schedule_cron` controls automatic scheduler evaluation:

| Value | Behavior |
|-------|----------|
| NULL, blank, `manual`, `@manual` | never auto-tick; only explicit scheduler IPC commands run it |
| `@tick` | due on every global daemon loop |
| five-field cron | due on the next matching local wall-clock minute |
| `@every <duration>` | due after that duration since the latest auto tick |

The global loop cadence is `AUWSX_TICK_SECS`, default 10 seconds with a floor of
1 second. A scheduler tick only evaluates state; it spawns an agent only when an
issue is actionable and project concurrency has a free slot.

## Policy groups

| Group | Fields |
|-------|--------|
| Identity | `name`, `repo_path`, `default_branch` |
| Agent commands | `main_agent_cmd`, `plan_agent_cmd`, `work_agent_cmd`, `review_agent_cmd` |
| Scheduling | `schedule_cron` |
| Soft gates | `plan_gate_timeout_min`, `completion_policy`, `completion_soft_timeout_min` |
| Hard caps | `iteration_timeout_min`, `main_job_timeout_min`, `review_max_rounds`, `conflict_max_attempts` |
| Integration | `max_concurrency`, `merge_mode` |
| Skills and maintenance | `skill_path`, `deepsleep_cron`, `last_deepsleep_at` |

Review command fallback is schema-level: `review_agent_cmd` NULL means use
`work_agent_cmd`. Project creation can use Arsenal presets to fill concrete
command strings, but project rows store the resolved commands.

## Profiles and global settings

Profiles are global organization, not per-project config:

| Concept | Rule |
|---------|------|
| profile table | name plus display order |
| project membership | one profile id plus order within that profile |
| moving profiles | append project to the target profile |
| global settings | profiles, Arsenal, default commands, and prompt defaults |
| project edit | remains project-specific runtime config |

## Routine output routing

The target model replaces vague routine `type` wording with output routing:

| Route | Behavior |
|-------|----------|
| `log` | keep run artifact/history only |
| `queue` | result may create pending backlog candidates |
| `note` | result may write knowledge/docs inside configured writable paths |

UI label should be "Output" or "Routes to". This is distinct from
`main_jobs.kind`, which records queued main-workspace work.

## Autonomous-operation recipe

For a project that should react quickly and complete without human gates:

| Field | Value |
|-------|-------|
| `schedule_cron` | `@tick` |
| `completion_policy` | `auto` |
| `plan_gate_timeout_min` | `0` |
| backlog admission | approved items only |

Anything else can park work at manual boundaries: NULL scheduling never
auto-runs, `manual` completion waits at ENDED, and positive plan gates delay
PLANNED before implementation.
