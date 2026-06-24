---
slug: issue-model
kind: domain
title: auwsx issue pipeline model (IssueStatus state machine)
description: The per-project issue pipeline with 16 IssueStatus states, scheduler classes, operator lanes, queue-message admission, and soft-gate rules.
keywords: [IssueStatus, scheduler class, state machine, ACTIONABLE HUMAN_GATED TERMINAL, status as synchronization marker, progress lane, attention marker, is_soft_gated, accepts_queue_message, queue message, backlog routing, FAILED, ABANDONED, check_transition, NEW, PLANNING, PLAN_READY, WORKING, REVIEWING, FIXING, AUDITING, READY_TO_MERGE, MERGING, RESOLVING_CONFLICT, CONFLICT_BLOCKED, completion policy, soft gate timeout, wait_until, crash resume, issue pipeline]
created: 2026-06-09
modified: 2026-06-24
---

# auwsx issue model

auwsx is a daemon whose scheduler drives coding agents against a **per-project
issue pipeline**. This file is the authoritative domain model (ratified in the
2026-06-09 decision ledger; it SUPERSEDES the stale `task` pipeline in the design
PRD `~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md` and the OLD
tmux/task model from 2026-06-04).

## Status as synchronization marker

The scheduler is a **ticker, not a process tracker**. Each tick reads each
issue's `status`:
- **Actionable** → spawn that phase's agent.
- **HumanGated** → wait.
- **Terminal** → archive.

The agent exits whenever it wants; whatever status it set (via the control CLI,
`auwsx issue ...`) before exiting decides continuation. **Crash-resume is free**:
a died agent leaves status untouched, so the next tick respawns it. auwsx NEVER
parses agent prose; agent→auwsx is the validated control CLI over IPC.

## States (16) + scheduler classes (3)

`src/state.rs::IssueStatus`; ids == the `issues.status` CHECK domain in
`0001_init.sql` (verified exact, both directions — see coding/db-schema.md).

| Class | States |
|-------|--------|
| **Actionable** (spawn agent) | New, Planning, Working, Reviewing, Fixing, Auditing, Merging, ResolvingConflict |
| **HumanGated** (wait) | PlanReady, PlanBlocked, ReviewBlocked, ReadyToMerge, ConflictBlocked |
| **Terminal** (archive, zero outgoing) | Done, Failed, Abandoned |

## Flow

```
NEW ─> PLANNING ─> PLAN_READY ─> WORKING ─> REVIEWING ⇄ FIXING
                         │                         │
                         │                         └─> AUDITING ─> READY_TO_MERGE
                         │                                  ^          │
                         │                                  └─ WORKING │
 PLAN_BLOCKED / REVIEW_BLOCKED / CONFLICT_BLOCKED (human)          MERGING ─> DONE
                                                                       │
                                                              RESOLVING_CONFLICT ⇄ CONFLICT_BLOCKED
 FAILED and ABANDONED are reachable from every non-terminal phase.
```

- Legal transitions are enforced by `state::check_transition`.
- `NEW→PLANNING` is daemon-owned phase entry before the planner prompt runs.
- **FAILED** and **ABANDONED** are reachable from every non-terminal phase.
- Terminal states (Done, Failed, Abandoned) have zero outgoing transitions.

## Phase semantics

| Phase | What happens | Worktree |
|-------|--------------|----------|
| NEW → PLANNING | daemon creates/records the issue worktree and enters PLANNING before spawning the plan agent. | created here |
| PLANNING → PLAN_READY | plan agent writes plan; PLAN_READY is a soft gate. | issue's worktree |
| WORKING | work agent codes. | issue's worktree |
| REVIEWING ⇄ FIXING | fresh REVIEW agent emits findings via `auwsx finding add`; implementer is re-spawned to adjudicate each (accept→fix / reject→rationale). Loop until clean or `review_max_rounds`. | issue's worktree |
| AUDITING → READY_TO_MERGE | final audit then merge gate. | issue's worktree |
| MERGING → DONE | merge; worktree torn down at DONE. | torn down |
| RESOLVING_CONFLICT ⇄ CONFLICT_BLOCKED | work agent attempts rebase/merge resolution; after `conflict_max_attempts` → CONFLICT_BLOCKED (human). | issue's worktree |

Deadlocks escalate to human gates: REVIEW after N rounds → REVIEW_BLOCKED;
conflict after N attempts → CONFLICT_BLOCKED; planning issue → PLAN_BLOCKED.

## Predicates

| Predicate | True for |
|-----------|----------|
| `is_soft_gated()` | **PlanReady** only. (ReadyToMerge is soft-gated only when `completion_policy='soft'` or auto-released when `completion_policy='auto'`, ORed in by the scheduler — not by the predicate.) |
| `accepts_queue_message()` target | **Planning, Working, Reviewing, Fixing, Auditing** |

- **Soft-gate-with-timeout** (`issues.wait_until`): PLAN_READY waits for human N min
  (`plan_gate_timeout_min`) then auto-advances.
- **Queue message** is the user-facing name for append-only guidance into active
  issue work. Target admission excludes `PLAN_READY`, blocked states,
  finalizing states, and terminal/archive states; joining new backlog into
  existing work should be visible through backlog history plus the target
  issue's queue messages, not as a separate joined issue card.

## Completion policy (per project)

| Policy | Behavior at READY_TO_MERGE |
|--------|-------------------|
| `manual` (default) | hard gate — human checks readiness; no auto-merge; human may add a queue message and move the issue back to WORKING |
| `soft` | timeout (`completion_soft_timeout_min`) then auto-advance |
| `auto` | merge immediately |

Agents that pass audit write/update `.auwsx/human-verify.md` before setting
READY_TO_MERGE. Keep that file as the compact, stable handoff: app run command,
pass/fail checks, and issue-specific behavior to inspect.

**Merge (local):** rebase-to-current (NEVER merge main into branch; no mid-branch
merges) + single `--no-ff` merge commit; post-merge `/dream` auto on default
branch. PR mode (`merge_mode='pr'`) = `/gh-pr`.

## Concurrency

Per-project concurrency is capped by `projects.max_concurrency`; N projects may
run in parallel.

## Triage / backlog

Triage auto-groups + promotes **APPROVED** backlog items into issues — no
grouping gate. The human gate is at **backlog admission** (see
domain/db-schema.md `backlog_items.source`+`approval`), not at grouping.
Promoted backlog retains its `consumed_issue_id` link for history, but normal
backlog listing returns only live, unpromoted items.

Manual issue removal is daemon-owned, not direct SQL: it refuses while an agent
is running, tears down the issue worktree if present, marks any source backlog
row dismissed so deletion does not resurrect old work, then deletes the issue
and cascading child rows.

Target routing model from 2026-06-18: approved backlog is routed by project-level
activity before issue creation. A standalone item becomes a new issue at `NEW` or
`PLANNING`; related active work attaches as a queue message to an attachable
issue. `CONSOLIDATING` is not an issue status, and `ABSORBED`/`JOINED` should be
backlog history rather than a Kanban card.

## User-facing lanes

`IssueStatus` stays the detailed scheduler marker. UI boards group those details
into four broad lanes:

| Lane | Detailed statuses |
|------|-------------------|
| PLAN | `NEW`, `PLANNING`, `PLAN_READY`, `PLAN_BLOCKED` |
| IN PROGRESS | `WORKING`, `REVIEWING`, `FIXING`, `REVIEW_BLOCKED`, `AUDITING` |
| FINALIZING | `READY_TO_MERGE`, `MERGING`, `RESOLVING_CONFLICT`, `CONFLICT_BLOCKED` |
| DONE | `DONE`, `FAILED`, `ABANDONED` |

TUI boards keep lane order fixed as PLAN, IN PROGRESS, FINALIZING, DONE. Issue
rows sort by id ascending inside each lane, so older work appears first. Backlog
items render before issue rows in PLAN; backlog ordering currently follows the
daemon-return order unless a future board-specific sort is added.

Operator attention is a separate marker from progress. Needs-attention statuses:
`PLAN_READY`, `PLAN_BLOCKED`, `REVIEW_BLOCKED`, `READY_TO_MERGE`,
`CONFLICT_BLOCKED`, and `FAILED`.
