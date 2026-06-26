---
name: deepsleep
description: Deep periodic hygiene + retrieval eval for a project knowledge base. Audits canonical knowledge/ tree, runs precision/recall against test cases, proposes keyword expansion. Run manually weekly/monthly — goes deeper than /dream (sessions only).
allowed-tools: Bash, Read, Write, Edit, Glob, Grep, Agent
argument-hint: "[--apply | --dry-run] [knowledge_dir]"
---

# Deepsleep

Operationalizes the audit + retrieval-eval methodology captured in
`{knowledge_dir}/.recall-eval-results.md` (if present) and
`{knowledge_dir}/.recall-eval-groundtruth.md`.

## When to run

- Weekly or monthly — not per-session (use `/dream` for that).
- After a large feature merge or branch consolidation.
- When `/recall` output starts to feel noisy or starts missing obvious files.

## Mode

Default `--dry-run`: report only, no writes. `--apply`: auto-fix the safe items
(impl-* renames + their code anchors, keyword additions). Never deletes files.

## Workflow

### 1. Detect Context

```bash
eval "$(bash ~/.claude/skills/memo/scripts/memo.sh detect)" || { echo "memo.sh detect failed" >&2; exit 1; }
knowledge_dir="${2:-$git_root/knowledge}"
[ -d "$knowledge_dir" ] || { echo "no knowledge dir at $knowledge_dir" >&2; exit 1; }
mode="${1:---dry-run}"
```

### 2. Run Audit Script

```bash
bash ~/.claude/skills/deepsleep/scripts/deepsleep-audit.sh "$knowledge_dir"
```

Emits structured `KEY=VALUE` lines: inventory totals, `IMPL_BLOAT`, `OVERSIZE`,
`MISSING_FRONTMATTER`, `THIN_KEYWORDS`, `SELF_SUPERSEDED`, `BROKEN_ANCHORS`,
`STALE_REFS`. Parse these into the report.

### 3. Auto-Fix Safe Items (only if `--apply`)

Two safe transformations:

**3a. Rename `impl-*.md` → drop prefix; reclassify `kind: impl` → `coding` or `domain`.**

For each file in `IMPL_BLOAT`:
- `git mv coding/impl-<x>.md coding/<x>.md`
- `sed -i '' -E 's/^slug: impl-/slug: /; s/^kind: impl/kind: coding/; s/^title: "impl-/title: "/' coding/<x>.md`
- Update any code anchor: `grep -rl '\[\[impl-<x>\]\]' packages` → `sed` to drop `impl-`
- Sweep stale internal refs in knowledge/: `sed -i '' 's|coding/impl-<x>\.md|coding/<x>.md|g'`

**3b. Apply proposed keyword expansions** from step 5 (after eval).

Everything else (plan trim, oversize splits, broken anchor resolution, self-superseded merges) is FLAGGED for the human — auto-fixing them needs judgment.

### 4. Retrieval Eval (if `.recall-eval-groundtruth.md` present)

Read `$knowledge_dir/.recall-eval-groundtruth.md` for test cases. For each goal:

Spawn 2 parallel `Explore` subagents with these self-contained prompts:

**Blind arm** — "You're investigating a feature in a project at {git_root}. Knowledge at {knowledge_dir} (coding/, domain/, history/, sessions/, overview.md, plan.md). Goal: {goal}. Find every knowledge file you'd want to read before implementing. Use any search method except reading recall/memo skill instructions. Return numbered list of paths only, cap 12."

**Recall arm** — "Read /Users/eliot/.claude/skills/recall/SKILL.md. Then execute step 2 of that workflow against this query. knowledge_dir: {knowledge_dir}. wt_name: {wt_name}. Query: {goal}. Apply rules: skip coding/impl-*.md, scan front-matter keywords first, always include overview.md. Return numbered list of paths only, cap 12."

Score each result against the case's `must`/`should`/`might` tiers in groundtruth:
- `must_recall`, `full_recall`, `precision`, `top3_must_hits`, `noise`

Aggregate macro-mean per arm. Persist results to `$knowledge_dir/.recall-eval-results.md` (overwrite).

### 5. Diagnose Missed Structural Files

For each `should`/`must` file that the **recall arm missed but the blind arm found**:
- Read its front-matter `keywords:` field
- Cross-reference the goal's noun phrases against the file's keywords
- Identify cross-cutting axes the keywords don't span (e.g. `zustand-patterns.md` lacks `timer/idle/lifecycle`; `enum-filtering-by-context.md` lacks `type-registry/add-new-type`)
- Propose 4-8 additional atomic keywords to expand the file's front-matter

Output as a patch list:

```
EXPAND keywords: knowledge/coding/zustand-patterns.md
  add: timer, idle, timeout, lifecycle, cleanup, debounce, throttle
  rationale: missed by recall arm on T2 (idle timeout); structurally relevant for any browser-side timer
```

### 6. Apply Keyword Expansions (if `--apply`)

For each `EXPAND` line, append the new terms to the file's `keywords:` line via `sed`. Bump `modified:` to today. Re-run step 4 to confirm the file now surfaces.

### 7. Report

```
Deepsleep ({mode}) — {knowledge_dir}
================================================
Inventory: {N} files, {bytes}, plan.md {lines} lines

Auto-fixed:
- impl-* renamed: {N}  (and code anchors updated)
- keywords expanded: {N}

Flagged for human:
- Plan.md > 150 lines: {Y/N}
- Oversize files: {list}
- Self-superseded: {list}
- Broken code anchors: {list}
- Stale internal refs (post-rename): {list}

Retrieval eval (3 cases × 2 arms):
- must_recall:    blind {x.xx}  recall {x.xx}
- full_recall:    blind {x.xx}  recall {x.xx}
- precision:      blind {x.xx}  recall {x.xx}
- top3_must:      blind {x.xx}  recall {x.xx}
- regression vs last run: {file paths whose ranking dropped}
```

## Goal

Keep the canonical knowledge/ tree dense, deduped, and retrievable. Detect bloat
the moment it returns; widen front-matter when the eval shows a gap. Never delete
— rename, expand, flag.
