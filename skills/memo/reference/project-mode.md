# Project Mode

## Docs-as-Code Mode (knowledge/ dir exists)

Knowledge files live in the project repo at `{git_root}/knowledge/`.

Structure:
```
knowledge/
  overview.md        # architecture, data flow, key abstractions, boundaries
  plan.md            # TODO / IN PROGRESS / COMPLETE
  domain/            # entities, relationships, terminology
  coding/            # patterns, conventions, impl docs
  sessions/          # per-worktree session dirs (created on first /learn)
    {wt_name}/
  history/           # consolidated implementation history (written by /dream)
```

### Session Files (branches — no direct knowledge edits)

Branches ONLY write session files. Stable knowledge files (`domain/`, `coding/`) are only modified on main via `/dream`.

Session path: `knowledge/sessions/{wt_name}/session-{timestamp}.md`

Worktree name detection:
```bash
wt_name=$(basename "$(git rev-parse --show-toplevel)")
# "main" if on main worktree; branch name or dir suffix if on worktree
```

Sessions include `target_slugs` — `/dream` uses these to consolidate into permanent files.

### Overview Update

Read/update `knowledge/overview.md`. Create from `learn/reference/BIGPICTURE-TEMPLATE.md` if missing.

Sections: What It Is / Architecture / Data Flow / Key Abstractions / Boundaries

Scan session for architectural changes → update affected sections. Set `last-verified: {YYYY-MM-DD}`.

### File Locations

| File | Content |
|------|---------|
| `coding/{topic}.md` | Reusable patterns, conventions (not feature-specific history) |
| `coding/impl-{feature}.md` | One per completed feature: how it works, choices, alternatives rejected |
| `domain/{topic}.md` | Entities, relationships, terminology |
| `history/{feature}.md` | Written by `/dream` after feature-complete merge |

All files have `slug` and `kind` frontmatter. Kind: `domain` | `coding` | `impl` | `overview` | `history`.

Add `// ^ [[slug]]` code comments on critical entry points (see `/docs-as-code`) — the knowledge file and code comment serve different purposes and coexist.

### Domain Extraction Check _(coding files only)_

After `/dream` writes `coding/` files, check each:
- References project entities or specific features? → skip
- Reusable pattern (language, library, general technique)? → extract

If extracting: copy (keep original in `knowledge/coding/`) to `notes/knowledges/{domain}/` in vault, create domain ir collection if new. Add to project copy:
```
> Canonical: [knowledges/{domain}/{filename}]
```

### Update Plan

File: `knowledge/plan.md`

```markdown
## TODO / ## IN PROGRESS / ## COMPLETE
- {item} _{added/started/done: YYYY-MM-DD}_
```

**Plan pruning**: collapse a `###` feature section when ALL of these are true: every item is `[x]`, AND the most recent `done:` date across all items in that section is > 4 weeks ago. Replace with archive line:
```markdown
## Archive
- **{Section Title}** — {one-line summary} _(done: YYYY-MM-DD)_
```

### Report Format

```
Saved:
- knowledge/sessions/{wt_name}/session-{date}.md (new)
  target_slugs: [slug1, slug2]
```

Or after `/dream`:
```
Dream complete:
- knowledge/domain/slug1.md (created)
- knowledge/coding/impl-slug2.md (updated)
- knowledge/history/feature.md (new)
```

---

## Vault Mode (modes 2 and 3, no knowledge/ dir)

Two sub-modes:
- **vault** (mode 2): `{vault_root}/notes/knowledges/{project}/` — synced, public
- **local-vault** (mode 3): `{vault_root}/notes-local/knowledges/{project}/` — private, git-ignored from vault

Privacy triggers for local-vault: project matches `kg*`, or user says "work"/"sensitive"/"local".

Since vault is global (no branching conflicts), `/learn` and `/recall` trigger `/dream` opportunistically when `session/` accumulates >3 files or oldest session >7 days.

Structure: `bigpicture/ coding/ domain/ session/ history/`

### Setup (first time only)

```bash
mkdir -p "$knowledge_path"/{domain,coding,session,bigpicture}
ir collection add "{collection_name}" "$knowledge_path"
```

### Big Picture

Read/update `{knowledge_path}/bigpicture/{project}-overview.md`.
Create from `learn/reference/BIGPICTURE-TEMPLATE.md` if missing.

Sections: What It Is / Architecture / Data Flow / Key Abstractions / Boundaries

### File Locations

| File | Content |
|------|---------|
| `coding/{topic}.md` | Reusable patterns, conventions |
| `coding/impl-{feature}.md` | Completed feature docs |
| `domain/{topic}.md` | Entities, relationships, terminology |

### Domain Extraction Check _(coding files only)_

After writing/updating `coding/` files, check each:
- Reusable pattern? → copy to `notes/knowledges/{domain}/`, add `> Canonical:` reference

### Update Plan

File: `{knowledge_path}/bigpicture/PLAN.md` — same format as docs-as-code mode above.

### Report Format

```
Saved:
- bigpicture/{project}-overview.md (updated — {what changed})
- bigpicture/PLAN.md (updated — {task moved})
- coding/impl-{feature}.md (new — {one-line})
- session/session-{date}.md (new)
```
