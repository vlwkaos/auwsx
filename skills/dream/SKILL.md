---
name: dream
description: Consolidate session files into permanent knowledge + housekeeping. project-mode=main branch only. Vault/local-vault=anytime.
allowed-tools: Bash, Read, Write, Edit, Glob, Grep
argument-hint: "[worktree name — optional, auto-detected]"
---

# Dream

Session consolidation + housekeeping. Always includes a domain extraction pass.

## When to Run

**project-mode** (`knowledge/` exists):
- **main**: full dream — consolidation + housekeeping + domain extraction
- **feature branch** (`--sessions-only`): compact sessions only; skip knowledge integration, plan, history, domain extraction, ir re-index. Triggered by `/recall` when >0 sessions on a feature branch.
- Triggers: first step of `/recall`, last step of `/learn` (on main), first step of `/release`

```bash
git branch --show-current
ls knowledge/sessions/*/session-*.md 2>/dev/null
```

**vault / local-vault**: no branch restriction. Run when:
- Session files exist in `{knowledge_path}/session/`
- Triggered by `/learn` (last step) or `/recall` (first step, if >3 sessions or oldest >7d)
- Explicit invocation (housekeeping pass even with no new sessions)

## Workflow

### 1. Detect Context

```bash
eval "$(bash ~/.claude/skills/memo/scripts/memo.sh detect)" || { echo "memo.sh detect failed" >&2; exit 1; }
wt_name=${ARGUMENTS:-$(basename "$git_root")}
```

All knowledge files (slugs, headings, content, sessions, plan.md, history entries) must be English.

- **project-mode, `--sessions-only`**: skip to [Sessions-Only Workflow](#sessions-only-workflow)
- **project-mode**: steps 2–7
- **vault / local-vault**: skip to [Vault Mode Workflow](#vault-mode-workflow)

### 2. Read Sessions

```bash
session_dir="$git_root/knowledge/sessions"
# scan ALL worktree subdirs — dream consolidates sessions from all merged branches
```

From all `session-*.md` under `$session_dir/*/`, extract per session: `target_slugs`, `plan_feature`, `kind`, source worktree dir, section content per `## {slug}` heading. Exit if no sessions found.

### 3. Consolidate into Knowledge Files

**Hard rule before dispatching subagents:**
- Drop any slug with `kind: impl` from coding/domain promotion. Impl-specific content stays in the session file (and is captured in the history entry — step 4a). Never write `coding/impl-*.md` or `domain/impl-*.md`.
- Drop the `impl-` prefix on any slug that classifies as a real reusable pattern; reclassify as `coding` or `domain`.
- Slug-merge: if a session slug is a near-synonym of an existing canonical slug (filename or keyword overlap), merge into the existing slug — never create a parallel near-duplicate.

Spawn one subagent per slug in parallel. Each subagent receives:
- `slug`, `kind`
- `session_content`: all session sections for this slug, each prefixed with `[YYYY-MM-DD]` (oldest first)
- `existing_file_path`: `knowledge/{kind}/{slug}.md` if file exists, else null

Subagent workflow (in order):
1. If `existing_file_path` set: read the full existing file
2. Decompose all content into discrete claims. Apply rules claim-by-claim:
   - Two session claims contradict each other → later-dated session wins
   - Session claim contradicts existing → session wins (recency)
   - Session claim is net-new → append
   - Claim already present (semantically) → no change
3. Write result:
   - New file: `knowledge/{kind}/{slug}.md`, standard frontmatter, `created` = now, `modified` = now. Required front-matter: `slug`, `kind`, `title`, `description` (one sentence), `keywords` (≥6 atomic terms — aliases, related concepts, function/type names, natural-language phrases). Empty/missing fields make the file unfindable.
   - Existing file: preserve original `created`, set `modified` = now; skip write if unchanged
   - File-size cap: ≤ 250 lines, dense (tables > prose, bullets > paragraphs). Split by sub-topic with cross-links beyond that.
4. Return: `{slug, action: created|updated|unchanged|error, path, reason?}`

Main agent collects results — never reads session files or knowledge file contents.

### 4. History + Plan + Annotations

#### 4a. Create History Entry (completed features only)

For each `plan_feature` judged complete, create `knowledge/history/{feature-slug}.md`:

```yaml
---
slug: {feature-slug}
kind: history
branch: {source-worktree}
completed: {YYYY-MM-DD}
---
# {Feature Name}

## Summary
{1-3 sentences}

## Key Decisions
{non-obvious choices and why}

## Knowledge Created/Updated
- `{kind}/{slug}.md` — {what changed}

## Implementation Notes
{condensed timeline from sessions}
```

Feature slug: derive from `plan_feature` frontmatter or branch name. Skip if no features complete.

#### 4b. Apply Plan Delta

`plan.md` — feature backlog, one `### {feature}` per feature, no task lists. Read `## plan` sections from all sessions and apply:

**Matching** (`plan_feature` → `### heading`):
1. Extract `#NNN` from both — numbers match → resolved
2. Normalize to lowercase, strip punctuation — ≥2 significant words overlap → resolved
3. No match → treat as new feature

**Per matched feature:**
- `completed` → delete `### {feature}` from plan.md (history entry is the record)
- `in-progress` → keep, update description if meaningfully different
- `blocked` → move to `## BLOCKED`, append reason

**New features** (session `new_features` list OR unmatched `plan_feature`): add as `### {slug}` under `## TODO`.

This is the only place plan.md is mutated. Feature branches never touch it directly.

#### 4c. Annotate Code Entry Points

Run `/docs-as-code` for each slug created/updated in step 3. Slug-only annotation (`// ^ [[slug]]`).

#### 4d. Migrate Legacy Markers (if any)

```bash
rg -l '// !' "$git_root/src" 2>/dev/null | xargs sed -i '' 's|// !|// ^|g'
```

### 5. Extract Domain Knowledge

Scan session content for reusable facts (language patterns, library techniques, framework concepts) not project-specific. Use Skill tool: `note` — pass topic + facts, type=knowledge. Skip if none found.

### 6. Clear Sessions

```bash
rm -f "{wt_dir}"/session-*.md
rm -f "{wt_dir}"/domain-staging.md   # content now in canonical domain/
rm -f "{wt_dir}"/coding-staging.md   # content now in canonical coding/
```

Do NOT delete staging files from worktree directories that had no sessions — those branches are still active.

### 6.5 Hygiene Sweep (project-mode, main only)

Run after step 6, before re-index. Goal: keep `knowledge/` from drifting back into bloat. Uses the same audit script as `/deepsleep` — one implementation, two callers.

```bash
bash ~/.claude/skills/deepsleep/scripts/deepsleep-audit.sh "$git_root/knowledge"
```

Parse the emitted `KEY=VALUE` lines:
- `IMPL_BLOAT=` — impl-* file in coding/ or domain/ → **auto-fix**: drop the `impl-` prefix, update the `slug` field, update any code anchors (indicates a skipped rename in step 3).
- `INVENTORY_PLAN_LINES=` — if >150, plan.md needs trimming (completed entries → history).
- `OVERSIZE=` — file >250 lines or >12K → candidate for split.
- `SELF_SUPERSEDED=` — file carrying a `Replaces`/`Superseded`/`Deprecated` marker → merge candidate.
- `BROKEN_ANCHOR=` — code anchor pointing to a missing slug file.
- `STALE_REF=` — knowledge file still referencing an `impl-*.md` path.

Report each as one line. Auto-fix only the `IMPL_BLOAT` renames. Plan trim, oversize splits, broken-anchor and stale-ref resolution surface as warnings — they need human judgment.

### 7. Update ir and Commit

Use Skill tool: `ir` — command reference.

```bash
ir update {project_name} && ir embed {project_name}
# if domain files written in step 5:
ir update {domain_collection} && ir embed {domain_collection}
```

```bash
git -C "$git_root" add knowledge/
git -C "$git_root" diff --cached --quiet || git -C "$git_root" commit -m "knowledge: dream {wt_name} - {YYYY-MM-DD}"
# if domain files written:
git -C $vault_root add notes/knowledges/{domain}/
git -C $vault_root diff --cached --quiet || git -C $vault_root commit -m "knowledge: domain {domain} - $(date +%Y-%m-%d)"
git -C $vault_root push
```

`git push` requires `dangerouslyDisableSandbox: true`.

### 8. Report

```
Dream complete for {wt_name}:
- Created: {N} files, Updated: {N} files
- History: knowledge/history/{feature-slug}.md
- Plan: {N} removed (complete), {N} updated, {N} new
- Domain extracted: {N} facts → {domain collection(s)}
- Sessions cleared: {N}
- ir re-indexed
```

---

## Sessions-Only Workflow

Feature branch session compaction + branch-level plan/task snapshot.

### S1. Read Sessions

```bash
session_dir="$git_root/knowledge/sessions/$wt_name"
ls "$session_dir"/session-*.md 2>/dev/null
```

- Exit if 0 sessions.
- 1 session: skip S2 (nothing to compact), skip S3, go directly to S2.5 → S2.6 → S4.

### S2. Compact

Merge all sessions for `$wt_name` into one consolidated file:
- Recency wins: later sessions override earlier for same topic/slug
- Retain all unique slugs and their latest state
- `## Trials & Solutions`: append all entries verbatim; never deduplicate or compress
- Line cap: 150 lines. If exceeded, summarize older slug content (keep headings + 1-line summary); never truncate `## Trials & Solutions`
- Output: `$session_dir/session-$(date +%Y%m%d)-consolidated.md`

### S2.5. Branch Plan Snapshot

If any sessions contain `## plan` sections, `plan_feature` frontmatter, or task checklists (`- [ ]`/`- [x]`), write/update `$session_dir/branch-plan.md`:

```markdown
---
branch: {wt_name}
updated: {YYYY-MM-DD}
plan_feature: {most recent plan_feature value}
---
# Branch Plan: {wt_name}

## In Progress
- {plan_feature}: {one-line description from sessions}

## Phase 1: {phase name}
- [x] completed task
- [ ] pending task

## Phase 2: {phase name}
- [ ] pending task

## Blocked
{items marked blocked with reason}

## Done (this branch)
Completed {N} tasks — {3-4 most recent by name}{ + N more}

## Trials & Solutions
- Tried {X} → {outcome} → {solution or workaround}
```

Rules:

| Rule | Detail |
|------|--------|
| Recency | Later session checklist state overrides earlier for same item |
| Phase grouping | Preserve original phase structure from `plan_feature`; no phases → single `## Pending` |
| Done summary | 3-4 most recent task names; collapse rest to `+ N more` |
| Trials & Solutions | Append verbatim; deduplicate exact duplicates only; never compress |
| Existing file | Merge: update phase task states, update Done summary count, append new Trials entries |
| Skip condition | No plan/task content found in any session |

`/recall` reads `branch-plan.md` unconditionally (alongside session files) to surface current task state without re-reading all raw sessions.

### S2.6. Regenerate Staging from Consolidated Session

Re-classify all `## {slug}` sections in the consolidated session using the same domain/coding/impl rules as memo step 3.5. Write fresh `domain-staging.md` and `coding-staging.md`, replacing any prior staging built from individual sessions. This keeps staging in sync after compaction.

### S3. Clear

```bash
rm -f "$session_dir"/session-*.md
mv "$session_dir/session-$(date +%Y%m%d)-consolidated.md" "$session_dir/"
# branch-plan.md, domain-staging.md, coding-staging.md retained (staging regenerated in S2.6)
```

### S4. Commit

```bash
git -C "$git_root" add knowledge/sessions/$wt_name/
git -C "$git_root" diff --cached --quiet || git -C "$git_root" commit -m "knowledge: dream $wt_name - $(date +%Y-%m-%d)"
```

### S5. Report

```
Sessions-only dream for {wt_name}:
- Compacted: {N} sessions → 1
- Slugs retained: {list}
- Branch plan: {created|updated|unchanged} (knowledge/sessions/{wt_name}/branch-plan.md)
```

---

## Vault Mode Workflow

`mode="vault"` or `mode="local-vault"`. No branch restriction.

### V1. Read Sessions

```bash
ls "$knowledge_path/session/session-"*.md 2>/dev/null
```

Skip if no sessions and running for housekeeping only.

### V2. Consolidate Sessions into Knowledge Files

Extract facts from each session and merge into `{knowledge_path}/coding/` and `{knowledge_path}/domain/`:

| Case | Action |
|------|--------|
| File exists | Merge new facts, replace superseded facts, update `modified` |
| File absent | Create with standard frontmatter (title, description, keywords, created, modified) |

### V3. Housekeeping Pass (always, even without new sessions)

Scan all files in `{knowledge_path}/coding/` and `{knowledge_path}/domain/`:

| Condition | Action |
|-----------|--------|
| >0.7 overlap between two files | Merge, keep oldest `created`, delete other |
| Outdated fact | Replace |
| Fully superseded file | Merge useful parts into surviving file, delete |
| Old context with historical value | Keep as timeline entry, mark current state at top |

### V4. Bigpicture + Plan Delta

- Sessions reference architectural changes → update `{knowledge_path}/bigpicture/{project_name}-overview.md`
- Apply plan delta (same logic as [4b](#4b-apply-plan-delta)) targeting `{knowledge_path}/bigpicture/PLAN.md`

### V5. Extract Domain Knowledge

Use Skill tool: `note` — pass topic + facts from session content, type=knowledge. Skip if no reusable general facts found.

### V6. Create History Entry

Write `{knowledge_path}/history/{feature-slug}.md` (same format as [4a](#4a-create-history-entry-completed-features-only)).

### V7. Clear Sessions

```bash
rm -f "$knowledge_path/session/session-"*.md
```

### V8. Index & Persist

Use Skill tool: `ir` — for correct command reference. `dangerouslyDisableSandbox: true` required.

```bash
ir update {project_name} && ir embed {project_name}
# if domain files written in V5:
ir update {domain_collection} && ir embed {domain_collection}
git -C $vault_root add notes/knowledges/{project_name}/ notes-local/knowledges/{project_name}/ notes/knowledges/{domain}/
git -C $vault_root diff --cached --quiet || git -C $vault_root commit -m "knowledge: dream {project_name} - $(date +%Y-%m-%d)"
git -C $vault_root push
```

`ir embed` and `git push` require `dangerouslyDisableSandbox: true`.

### V9. Report

```
Dream complete for {project_name} (vault):
- Sessions consolidated: {N}, cleared
- Files updated: {N}, merged: {N}, deleted: {N}
- Domain extracted: {N} facts → {domain collection(s)}
- History: history/{feature-slug}.md
- bigpicture: {updated|unchanged}
- ir re-indexed
```

---

## Session Format Reference

@reference/session-format.md
