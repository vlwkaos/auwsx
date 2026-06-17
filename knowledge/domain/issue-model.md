---
slug: issue-model
kind: domain
title: auwsx issue pipeline model (IssueStatus state machine)
description: The per-project issue pipeline — 16 IssueStatus states, 3 scheduler classes, 37 transitions, status-as-sync-marker, and the soft-gate/working-phase/absorbed/failed rules that drive the daemon scheduler.
keywords: [IssueStatus, scheduler class, state machine, ACTIONABLE HUMAN_GATED TERMINAL, status as synchronization marker, is_soft_gated, accepts_steering, is_working_phase, ABSORBED, FAILED, check_transition, CONSOLIDATING, PLANNING, IMPLEMENTING, REVIEW NEEDS_FIX loop, CONFLICTED, COMPLETING, completion policy, soft gate timeout, wait_until, crash resume, issue pipeline]
created: 2026-06-09
modified: 2026-06-09
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
| **Actionable** (spawn agent) | Consolidating, Planning, Implementing, Review, NeedsFix, Audit, Conflicted, Completing |
| **HumanGated** (wait) | Planned, PlanBlocked, ReviewBlocked, ConflictBlocked, Ended |
| **Terminal** (archive, zero outgoing) | Done, Absorbed, Failed |

## Flow

```
CONSOLIDATING ─┬─> PLANNING ─> PLANNED ─> IMPLEMENTING ─> REVIEW ⇄ NEEDS_FIX
               │                                            │
               └─> ABSORBED                                 └─> AUDIT ─> ENDED
                                                                          │
   PLAN_BLOCKED / REVIEW_BLOCKED / CONFLICT_BLOCKED (human)          COMPLETING ─> DONE
                                                                          │
                                                                     CONFLICTED ⇄ CONFLICT_BLOCKED
   FAILED reachable from every non-terminal phase.
```

- **37 legal transitions** enforced by `state::check_transition`.
- **ABSORBED** only from CONSOLIDATING (and only via `mark_absorbed`).
- **FAILED** reachable from every non-terminal phase.
- Terminal states (Done, Absorbed, Failed) have zero outgoing transitions.

## Phase semantics

| Phase | What happens | Worktree |
|-------|--------------|----------|
| CONSOLIDATING | main agent checks for a similar in-flight issue in a WORKING phase. Found → fold this task in as steering, self-close to **ABSORBED** (no new worktree). Else → standalone → PLANNING. | none yet |
| PLANNING → PLANNED | plan agent writes plan; PLANNED is a soft-gate. | worktree created at CONSOLIDATING→PLANNING (standalone only; delegated tasks reuse target's) |
| IMPLEMENTING | work agent codes. | issue's worktree |
| REVIEW ⇄ NEEDS_FIX | fresh REVIEW agent (3rd eye + devil's-advocate) emits findings via `auwsx finding add`; implementer RE-SPAWNED to adjudicate each (accept→fix / reject→rationale). Loop until clean or `review_max_rounds`. | issue's worktree |
| AUDIT → ENDED | final audit then ENDED gate. | issue's worktree |
| CONFLICTED ⇄ CONFLICT_BLOCKED | work agent attempts rebase/merge resolution; after `conflict_max_attempts` → CONFLICT_BLOCKED (human). | issue's worktree |
| COMPLETING → DONE | merge + post-merge `/dream`; worktree torn down at DONE. | torn down |

Deadlocks escalate to human gates: REVIEW after N rounds → REVIEW_BLOCKED;
conflict after N attempts → CONFLICT_BLOCKED; planning issue → PLAN_BLOCKED.

## Predicates

| Predicate | True for |
|-----------|----------|
| `is_soft_gated()` | **Planned** only. (ENDED is soft-gated only when `completion_policy='soft'`, ORed in by the scheduler — not by the predicate.) |
| `accepts_steering()` == `is_working_phase()` | **Implementing, Review, NeedsFix, Audit** |

- **Soft-gate-with-timeout** (`issues.wait_until`): PLANNED waits for human N min
  (`plan_gate_timeout_min`) then auto-advances.
- **Steering** = append-only guidance into a WORKING-phase issue. NEVER edits
  plan.md; sets `has_pending_steering=1` to re-trigger the scheduler. Sources:
  human, consolidation (delegation).

## Completion policy (per project)

| Policy | Behavior at ENDED |
|--------|-------------------|
| `manual` (default) | hard gate — human checks COMPLETE; no auto-merge |
| `soft` | timeout (`completion_soft_timeout_min`) then auto-advance |
| `auto` | merge on ENDED |

**Merge (local):** rebase-to-current (NEVER merge main into branch; no mid-branch
merges) + single `--no-ff` merge commit; post-merge `/dream` auto on default
branch. PR mode (`merge_mode='pr'`) = `/gh-pr`.

## Concurrency

Serial-per-project v1 (`max_concurrency=1`, one active issue/worktree); N
projects in parallel. Intra-project parallelism is a later concern.

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

## User-facing lanes

`IssueStatus` stays the detailed scheduler marker. UI boards group those details
into four broad lanes:

| Lane | Detailed statuses |
|------|-------------------|
| TODO | `CONSOLIDATING`, `PLANNING`, `PLANNED`, `PLAN_BLOCKED` |
| IN PROGRESS | `IMPLEMENTING`, `NEEDS_FIX`, `COMPLETING`, `CONFLICTED`, `CONFLICT_BLOCKED` |
| REVIEW | `REVIEW`, `REVIEW_BLOCKED`, `AUDIT`, `ENDED` |
| COMPLETE | `DONE`, `ABSORBED`, `FAILED` |
