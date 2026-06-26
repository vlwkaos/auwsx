# Session File Format

## Docs-as-Code Mode

### Path

```bash
timestamp=$(date +%Y-%m-%d-%H%M)
git_root=$(git rev-parse --show-toplevel)
wt_name=$(basename "$git_root")
session_path="$git_root/knowledge/sessions/$wt_name/session-$timestamp.md"
mkdir -p "$(dirname "$session_path")"
```

### Frontmatter

```yaml
---
title: "session-{timestamp}"
description: {what this session teaches}
target_slugs: [slug-one, slug-two]
plan_feature: {feature-name}   # matches ### heading in plan.md
kind: domain|coding|impl
keywords: {5+ keywords}
created: "YYYY-MM-DDTHH:mm:ss"
modified: "YYYY-MM-DDTHH:mm:ss"
author: vlwkaos
tags:
  - knowledge
  - {collection}
---
```

`target_slugs` declares which knowledge files this session will produce when `/dream` is run. Required for consolidation.
`plan_feature` is a slug-style identifier linking this session to a feature entry in `plan.md`. Used by `/dream` for fuzzy matching — see matching rules below.

### Content Structure

Use `## {slug}` sections for each target slug, plus a `## plan` section:

```markdown
## auth-flow
Content that will become knowledge/domain/auth-flow.md...

## jwt-validation
Content for knowledge/coding/jwt-validation.md...

## plan
status: in-progress | completed | blocked
new_features:
  - slug: next-feature-name
    description: one-line description
notes: {key decisions, blockers, context for this feature}
```

`plan.md` is a **feature backlog** — no granular task lists. The `## plan` section signals feature-level status only.
- `status: completed` → dream removes the feature from plan.md and writes a history entry
- `status: in-progress` → dream updates description if notes provide new context
- `status: blocked` → dream moves feature to `## BLOCKED` with notes as reason
- `new_features` → dream adds each as a new `### {slug}` entry under `## TODO`

### plan_feature Matching Rules (used by `/dream`)

Dream resolves `plan_feature` to a `### heading` in plan.md using two passes:

1. **Issue number**: extract `#NNN` from both — if numbers match, resolved
2. **Slug overlap**: normalize both to lowercase, strip punctuation — if ≥2 significant words overlap, resolved
3. **No match**: treat as new feature, add under `## TODO` with session description as body

### Staging Files (feature branches only)

Written by `/memo` step 3.5. Live alongside session files in `sessions/{wt_name}/`:

| File | Content |
|------|---------|
| `domain-staging.md` | Domain sections extracted from sessions (entities, relationships, rules) |
| `coding-staging.md` | Coding sections extracted from sessions (patterns, techniques, conventions) |

These bridge the gap between branch sessions and `/dream` consolidation. `/recall` loads them with the same priority as canonical `knowledge/domain/` and `knowledge/coding/` files.

Format:
```markdown
---
kind: domain-staging   # or coding-staging
wt_name: {wt_name}
updated: {ISO timestamp}
sources: [session-2026-04-29-1320.md, session-2026-04-29-1540.md]
---

## {slug}

{content}

## {another-slug}

{content}
```

Lifecycle: created by `/memo` on first branch session; updated (merge) on each subsequent `/memo`; deleted by `/dream` after consolidating into canonical `knowledge/domain/` and `knowledge/coding/`.

### Rules

- Knowledge files are English-only: slugs, headings, content, `plan_feature` values
- No rotation — sessions accumulate until `/dream` consolidates them
- Sessions are branch-namespaced — no merge conflicts by design
- Do NOT directly edit `knowledge/domain/`, `knowledge/coding/`, or `knowledge/plan.md` on feature branches
- All shared knowledge mutations flow through: session (feature branch) → staging (branch-local) → dream (main)

---

## Legacy Vault Mode

### Path

```bash
timestamp=$(date +%Y-%m-%d-%H%M)
```

| Mode | Path |
|------|------|
| Project | `{knowledge_path}/session/session-{timestamp}.md` |
| Domain | `{knowledge_path}/session-{timestamp}.md` (flat) |

### Frontmatter

```yaml
---
title: "session-{timestamp}"
description: {what this session teaches}
keywords: {5+ keywords}
extracted_to:
  - {file created or updated}
created: "YYYY-MM-DDTHH:mm:ss"
modified: "YYYY-MM-DDTHH:mm:ss"
access: public|local
author: vlwkaos
tags:
  - knowledge
  - {collection}
---
```

### Rotation

Keep max 20 session files per collection. Delete oldest if over.

```bash
ls -t {knowledge_path}/session/session-*.md | tail -n +21 | xargs rm -f
```
