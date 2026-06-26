---
name: memo
description: Save project knowledge after a task or session. Writes session files for /dream; delegates personal notes to /note.
allowed-tools: Bash, Read, Write, Edit, Glob, Grep
---

# memo

## Context-continuation contract

Use `/memo` as the durable continuation step before context resets. When a session is approaching context pressure or the user intends to `/clear`, save the important deltas first: decisions, changed files, blockers, commands that matter, unresolved next steps, and reusable methods. Then `/dream` may consolidate if thresholds are met; after `/clear`, resume with `/recall <goal>`. This KB flow is preferred over relying on opaque compression for long-running project continuity.

Do not memoize generic one-off task narratives or transient setup failures. Save only durable project/session deltas or reusable fixes/patterns.

## Dispatch

| Trigger | Action |
|---------|--------|
| "save a note" / "personal" / "learning" / "untie from project" | Delegate to `/note`, stop |
| In a project context (git repo) | Knowledge mode → below |
| No git context | Vault mode → below |

## 1. Detect Context

```bash
bash ~/.claude/skills/memo/scripts/memo.sh detect
```

Outputs: `mode`, `git_root`, `project_name`, `vault_root`, `knowledge_path`.

- `mode=none`: no knowledge store exists yet. Ask user which mode, scaffold it, then re-run detect:
  ```bash
  # project-mode
  mkdir -p "$git_root/knowledge"/{domain,coding,sessions,history}
  # vault (shared) or local-vault (private — use notes-local/ instead of notes/)
  mkdir -p "$vault_root/notes/knowledges/$project_name"/{bigpicture,domain,coding,session,history}
  ir collection add "$project_name" <knowledge-path>
  ```

### Fallback when shell/terminal is blocked

If `memo.sh detect` cannot run due to terminal denial/policy:
- Do not stop. Continue in manual fallback mode.
- Infer project mode from readable paths:
  - project-mode candidate: `<repo>/knowledge/`
  - vault-mode candidate: `<vault>/notes/knowledges/<project>/`
  - local-vault candidate: `<vault>/notes-local/knowledges/<project>/`
- If only one candidate exists, proceed with that mode.
- If multiple or none exist, ask user to choose mode.
- Record the fallback decision and blocker in session `## Trials & Solutions`.

## 2. Search Before Writing

Spawn Agent subagent (do NOT use Skill tool — keeps knowledge-search SKILL.md out of main context):

```
Search for "{2-3 keywords from session}" in project knowledge.
knowledge_dir: {knowledge_dir}    wt_name: {wt_name}
1. ls {knowledge_dir}/domain/ — filter by filename relevance
2. ls {knowledge_dir}/coding/ — filter by filename relevance
3. ls {knowledge_dir}/sessions/{wt_name}/ — read each file, scan ## slug headings for matches
4. Always include {knowledge_dir}/overview.md
Rule: do NOT add "agent" as keyword unless query explicitly asks about agents/AI.
Return: numbered list — {n}. {path} — {one-line relevance note}
```

Use returned path list to decide: update / create / skip per target file. Do NOT read files in main context.

## 3. Save Project Knowledge

**Front-matter required on every file** (session, staging, canonical): `slug`, `kind`, `title`, `description` (one sentence), `keywords` (≥6 atomic terms covering aliases, related concepts, function/type names, and natural-language phrases a future `/recall` is likely to use), `created`, `modified`. Empty/missing fields make the file unfindable.

**File-size cap on `coding/` and `domain/` files**: ≤ 250 lines, dense format (tables > prose, bullets > paragraphs). Beyond that, split by sub-topic with sibling cross-links.

**plan.md is backlog only**: `### {feature}` + 1-line description; no checklists, no completion records. Anything completed → `history/{feature-slug}.md` and removed from plan.

**project-mode** — write session file only (@reference/session.md).

Path: `{knowledge_dir}/sessions/{wt_name}/session-{timestamp}.md`

Include: `target_slugs`, `plan_feature`, `## {slug}` sections, `## plan` delta.

**Branch plan capture** (feature branches) — always include `## pending`:
- `- [ ]` unchecked items for everything not yet done, grouped by phase if plan has phases
- `- [x]` checked items for tasks completed this session
- Blocked items with reason

**Trials & Solutions capture** — if session involved trial-and-error, failed attempts, or non-obvious solutions, include `## Trials & Solutions`:
- One bullet per trial: `Tried {X} → {outcome} → {solution or workaround}`
- Never omit: feeds `/dream` knowledge accumulation; do not compress

This feeds `/dream --sessions-only` to build `branch-plan.md`. Without it, branch-level task state is lost between sessions.

Do NOT edit `knowledge/domain/`, `knowledge/coding/`, `knowledge/plan.md` directly — all mutations flow through `/dream` on main.

**3.5 Stage Domain/Coding Knowledge (feature branch only — skip on main)**

After writing the session file, extract reusable knowledge so `/recall` can find it immediately without waiting for `/dream`.

Classify each `## {slug}` section:

| Kind | Criteria | Destination |
|------|----------|-------------|
| `domain` | Entities, relationships, terminology, enums, types, business rules, schemas | `sessions/{wt_name}/domain-staging.md` |
| `coding` | Patterns, techniques, library/framework behavior, conventions, implementation recipes | `sessions/{wt_name}/coding-staging.md` |
| `impl` | Task progress, component state, feature-specific UI decisions, pending items | Session file only — do NOT stage |

**Hard rule — no `impl` files in `coding/` or `domain/`.** A slug is either reusable (drop the `impl-` prefix, classify as `coding`/`domain`) or it is impl-only and stays in the session file. `impl-*.md` accumulating in `coding/` is the #1 bloat anti-pattern.

**Slug naming**: lowercase, hyphenated, noun-or-pattern phrase. No `impl-` prefix. No date stamps. No issue numbers in the slug (use them in title/description instead).

**Dedupe-on-write**: before staging a `## {slug}` section, check whether a canonical file already exists for the same topic (filename match OR keyword overlap from step 2 results). If yes, the new section must clearly extend or correct it — do NOT create a near-duplicate slug under a slightly different name. Reuse the existing slug.

**Source selection** (check before writing):
- Staging files absent → scan ALL `session-*.md` in `sessions/{wt_name}/` (one-time migration from old sessions) + current session
- Staging files present → current session only

Merge rules (read existing staging file if present):
- Slug present → replace entire section (recency wins)
- New slug → append section

Staging file format: @reference/session.md → "Staging Files" section.

Update `sources` list to include processed session filenames.

**vault / local-vault** — extract directly into knowledge files.

Identify target files from step 2 results (coding/, domain/, bigpicture/PLAN.md; overview if architecture changed). Compose `new_content` per file, then spawn one subagent per file in parallel. Each subagent receives:
- `new_content`: content to integrate
- `existing_file_path`: path if exists, else null
- `target_path`: write destination
- `kind`: coding | domain | plan | overview

Subagent workflow:
1. If `existing_file_path` set: read the full existing file
2. Decompose all content into discrete claims. Apply claim-by-claim: present semantically → no change; contradicts existing → new wins; net-new → append; superseded but historically relevant → keep as dated timeline entry
3. Write to `target_path`. File quality: dense, tables > prose, bullets > paragraphs, ≤100 lines.
4. Return: `{file: target_path, action: created|updated|unchanged|error}`

Main agent collects manifest — never reads existing knowledge files.

Write session file directly (no merge): `{knowledge_path}/session/session-{timestamp}.md`

## 4. Extract Domain Knowledge

If session contains reusable general knowledge (language patterns, library techniques, framework concepts) not project-specific:

Use Skill tool: `note` — pass topic and facts, type=knowledge

## 5. Index & Persist

```bash
bash ~/.claude/skills/memo/scripts/memo.sh persist $mode $git_root $project_name
```

Requires `dangerouslyDisableSandbox: true` (`ir embed`, `git push`).

## 6. Trigger /dream

```bash
dream_args=$(bash ~/.claude/skills/memo/scripts/memo.sh dream-check $mode $knowledge_path $project_name)
```

- exits 1 → skip
- exits 0, `--sessions-only` (feature branch) → spawn Agent subagent (keeps dream SKILL.md out of main context):
  ```
  Compact sessions for {wt_name} in {git_root}.
  Session dir: {git_root}/knowledge/sessions/{wt_name}/
  1. Count session-*.md: 0→exit; 1→skip to step 3
  2. Merge all into consolidated: latest slug wins; all Trials & Solutions verbatim; cap 150 lines
     Write: sessions/{wt_name}/session-{date}-consolidated.md; delete originals
  3. Write/update branch-plan.md (phase checkboxes, 3-4 done + N more, all Trials & Solutions verbatim)
  4. Reclassify slugs → domain-staging.md (entities/rules/schemas) | coding-staging.md (patterns/recipes) | skip (impl/UI)
  5. git -C {git_root} add knowledge/sessions/{wt_name}/ && git commit -m "knowledge: dream {wt_name} - {date}"
  Requires dangerouslyDisableSandbox: true
  ```
- exits 0, empty (main branch) → spawn Agent subagent:
  ```
  Consolidate all sessions into knowledge/ for {project_name} at {git_root}.
  1. Read all sessions/*/session-*.md; group by slug; latest date wins
  2. Write/update knowledge/{kind}/{slug}.md per slug (domain or coding)
  3. Update knowledge/plan.md: completed→delete; new→add under ## TODO
  4. Write knowledge/history/{feature-slug}.md for each completed plan_feature
  5. Delete all session-*.md and staging files from merged worktrees
  6. ir update {project_name} && ir embed {project_name}
  7. git -C {git_root} add knowledge/ && git commit -m "knowledge: dream {wt_name} - {date}"
  Requires dangerouslyDisableSandbox: true
  ```
