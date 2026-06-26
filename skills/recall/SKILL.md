---
name: recall
description: Load project context before starting work on a task — architecture, patterns, active tasks.
allowed-tools: Bash, Read, Write, Edit, Glob, Grep
argument-hint: "[topic or task]"
---

# Recall

> For quick lookups (single term/concept, no planning) use `/seek <topic>` — same tier as grep. `/recall` is for full planning context only.

## Deterministic trigger contract

- If a session starts with a clear project/coding/debugging/research goal, run `/recall <goal>` before planning, editing, or answering architecture questions.
- Skip heavy recall for small literal file/function questions, pure syntax questions, or no-goal chat; use file/code tools directly unless a project concept is unclear.
- If unsure whether local knowledge may contain the answer, prefer `/seek` or `/recall` before guessing; ask the user only after retrieval cannot resolve the ambiguity.
- For exact prior conversation wording, use transcript/session search rather than project knowledge recall.
- For continuation after `/clear`, first action is `/recall <continuation goal>` so the reconstructed context comes from curated knowledge, branch sessions, and staging files.
- Hermes memory/skill architecture note: see `references/hermes-memory-skill-hybrid-evaluation.md` for the evaluated Hybrid-minimal target where this KB flow is primary and Hermes keeps only tiny USER.md, session_search, write gates, and minimal meta-skills.

## Task

$ARGUMENTS

## Goal

- Avoid cross-project bleed unless user or project instructions explicitly route there.

## Workflow

### 0. Trigger Dream

```bash
bash ~/.claude/skills/memo/scripts/memo.sh detect
git branch --show-current
```

| mode | branch | action |
|------|--------|--------|
| project-mode | main | spawn Agent with full dream prompt (see memo step 6) |
| project-mode | feature | `ls knowledge/sessions/{wt_name}/session-*.md 2>/dev/null \| wc -l` → if >0 → spawn Agent with --sessions-only prompt (see memo step 6) |
| vault / local-vault | any | `bash ~/.claude/skills/memo/scripts/memo.sh dream-check $mode $knowledge_path` → exit 0 → spawn Agent with full dream prompt |

### 1. Detect Mode

```bash
eval $(bash ~/.claude/skills/memo/scripts/memo.sh detect)
# sets: mode, git_root, project_name, vault_root, knowledge_path
```

### 2. Load Project Knowledge

**Mandatory first action.** Run this Bash command verbatim (do not substitute reads or grep — this is the only ir entry point in /recall):

```bash
bash ~/.claude/skills/seek/scripts/seek.sh "$ARGUMENTS"
```

Runs in the default sandbox. Capture the ranked numbered list — these are the ir semantic hits (project + domain collections) plus filename fallback. If you skip this step, the synthesis subagent has no ir-derived files to read.

Then assemble project-mode-specific files /seek does not return:
- All `sessions/{wt_name}/session-*.md` — read for `## slug` headings matching `$ARGUMENTS`
- Always-include: `{knowledge_dir}/overview.md`
- Drop `coding/impl-*.md` and `domain/impl-*.md` from /seek results (legacy bloat)

Spawn one synthesis subagent with `query`: $ARGUMENTS and `file_paths` per mode:

| mode | include |
|------|---------|
| project-mode (main) | `{knowledge_dir}/overview.md`, `plan.md`, all `sessions/{wt_name}/session-*.md`; domain/coding top 5 ≥ 0.15 |
| project-mode (feature) | `{knowledge_dir}/overview.md`, `sessions/{wt_name}/branch-plan.md` (if exists), all `sessions/{wt_name}/session-*.md`; `sessions/{wt_name}/domain-staging.md` (if exists); `sessions/{wt_name}/coding-staging.md` (if exists); canonical domain/coding top 5 ≥ 0.15. Skip root `plan.md` |
| vault / local-vault | all `{knowledge_path}/bigpicture/` (not `session/`); domain/coding top 5 ≥ 0.15 |

**Staging files** (`domain-staging.md`, `coding-staging.md`): feature-branch only; same authority as canonical domain/coding. Priority: session > staging > canonical.

**Subagent instructions:**
Read every file in `file_paths` completely. Then:

1. Extract facts, constraints, decisions, and code patterns needed to work on `{query}`.
2. Skip content unrelated to `{query}` — other services, unrelated features, background prose.
3. Identify implementation blockers: questions the task requires answering that no file resolves.
4. Hard cap: 10,000 tokens.

Return exactly:
```
## Relevant Knowledge
{synthesized facts — tables/bullets, no filler}

## Active Constraints
{rules and invariants that apply to this task}

## Recent Decisions
{session entries relevant to task, newest first}

## Open Questions
{facts needed to implement this task that are absent from all files — one line each}
```

Main agent receives synthesis only — never reads source files.

### 2.5. Explicit Collection Override

If the user names an `ir` collection: confirm it exists (`ir collection ls`), then pass it positionally to /seek: `seek.sh "$ARGUMENTS" <col>`. If not found, say so and continue normally.

Do NOT infer sibling projects from repo name alone — cross-project recall requires explicit user instruction or project instructions.

### 2.6. Schema-First Recall

If `$ARGUMENTS` concerns enums, types, schema, properties, validation rules, or code/value mappings:

| Priority | Source |
|----------|--------|
| 1st | `domain/*.md`, `schema*.md`, `*type*.md`, `*enum*.md`, coding rule docs |
| 2nd | Sessions/meeting notes (supporting context only) |

Do not stop on inferred mappings when a canonical source likely exists.

### 3. Load Domain Knowledge

/seek in Step 2 already searches domain collections by project signal (Cargo.toml→rust, package.json→typescript/react, .claude→claude-code). For ad-hoc domains not auto-detected (ai, terminal, git, database, etc.), re-run /seek with positional extras:

```bash
bash ~/.claude/skills/seek/scripts/seek.sh "$ARGUMENTS" <extra-col> <extra-col>
```

Top 3 at ≥ 0.15.

### 4. Synthesize

| Class | Source |
|-------|--------|
| `authoritative` | domain/schema/coding docs |
| `supporting` | project knowledge, implementation docs |
| `context` | meetings/sessions/notes |

Include provenance when relevant. If answer depends on inference, say so and name the missing authoritative source.

### 5. Change-Surface Awareness

If `$ARGUMENTS` names specific files and context doesn't cover the topic:

```bash
rg '\[\[[a-z][a-z0-9-]*\]\]' {file}
```

Load matching `knowledge/{kind}/{slug}.md` — max 2 files.

### 6. Update Plan

Skip if `$ARGUMENTS` empty or on feature branch (project-mode).

`plan.md` = feature backlog, `### {feature}` + one-line description. No task lists.

| mode | path |
|------|------|
| project-mode (main) | `{git_root}/knowledge/plan.md` |
| vault / local-vault | `{knowledge_path}/bigpicture/PLAN.md` |

- TODO → IN PROGRESS: move entry
- not found → add under `## IN PROGRESS`
- already IN PROGRESS → no change

Do NOT remove/complete/archive — `/dream` owns that.

### 7. Execute

- Empty `$ARGUMENTS`: summarize project state (architecture, active tasks, recent changes). Stop.
- Non-empty: surface any Open Questions to the user before implementing — do not infer answers. Then plan and execute.

If AGENTS.md has stale recall/memo instructions, update in-place.

### 8. Hygiene Flag (project-mode only, silent unless dirty)

After load, run quick checks against `knowledge/`. If any fail, emit one short warning line at the end of the recall output (do not block):

```bash
impl_in_coding=$(ls {knowledge_dir}/coding/impl-*.md 2>/dev/null | wc -l)
plan_lines=$(wc -l < {knowledge_dir}/plan.md 2>/dev/null)
oversize=$(find {knowledge_dir}/coding {knowledge_dir}/domain -name '*.md' -size +12k 2>/dev/null | head -3)
```

Warn if: `impl_in_coding > 0` (suggest /dream-driven rename), `plan_lines > 150` (suggest plan trim), `oversize` non-empty (suggest split). One-liner per issue, no blocking.
