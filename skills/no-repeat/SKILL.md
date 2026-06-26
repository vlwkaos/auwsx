---
name: no-repeat
description: After hitting a problem (bug, wrong assumption, wasted retry, missed gotcha), place a smallest-footprint discoverable hint so a future agent/human visiting the same code or task will not repeat it. Default = inline `^` comment AT the offending code line, optionally pointing to /seek keywords. Complex/multi-faceted -> /memo. Project policy -> AGENTS.md. Cross-project -> global CLAUDE.md.
allowed-tools: Bash, Read, Edit, Write
argument-hint: "<one-line summary of the gotcha>"
---

# no-repeat - leave a future-proof breadcrumb

Goal: pick the placement with the **smallest context footprint** that the **future visitor will reliably encounter** at the moment they need it.

## Decision tree

Walk top-to-bottom. First match wins. Skip the rest.

| # | Condition | Placement | Footprint |
|---|-----------|-----------|-----------|
| 1 | Caused by a specific code line/block | inline `^` comment AT that line | nil context cost; visitor sees it when reading the code |
| 2 | Caused by a directory/module pattern | `^` comment at the file's top OR README inside that dir | local |
| 3 | Project-wide rule (always-on) | append rule line to repo `AGENTS.md` / `CLAUDE.md` | small global-to-project |
| 4 | Cross-project habit / global rule | append to `~/.claude/CLAUDE.md` (current `Guidelines` block) | global, always loaded - use sparingly |
| 5 | Long context / multi-faceted / non-code | `/memo` with full detail; leave 1-line `^ see /seek <keywords>` pointer near nearest code if any | KB-only |
| 6 | Library/framework behavior, not project-specific | `/note` (vault knowledge) | personal KB |

**If two placements both fit, pick the more local one. Only add a higher-level rule if the gotcha already happened in 2+ places.**

## Comment shape (rules 1, 2, 5)

- Prefix `^` (matches the "necessary-only comments marked with `^`" rule).
- One line if possible. State the gotcha, not the fix.
- Short context -> inline it.
- Long context -> `^ see /seek <2-3 keywords>` (route to KB, not inline).

Examples:
```rust
// ^ ir search returns no hits if --min-score > 0.15 for short queries
let res = ir.search(q).min_score(0.10);
```
```ts
// ^ see /seek portal-type-registry - adding a new type requires 3 edits
const portal = registerPortal(...);
```

## Steps

1. **Diagnose the recurrence risk.** What exactly was the wrong path? What signal would have prevented it?
2. **Pick the placement** using the table above. State the choice in one sentence before editing.
3. **Apply.** Edit / append / `/memo` / `/note`. Do not redundantly write to multiple tiers - only the chosen tier, plus an optional one-line pointer.
4. **Verify discoverability.** If the placement is a comment + KB pointer, confirm `/seek <keywords>` actually surfaces the KB entry. If not, adjust the slug/keywords until it does.

## Do NOT

- Do NOT default to AGENTS.md - that bloats every future context. Use only for true project-wide rules.
- Do NOT default to /memo for code-local gotchas - the future visitor reads the code, not the KB.
- Do NOT write multi-line explanations as comments. Long -> KB, short pointer in code.
- Do NOT add to `~/.claude/CLAUDE.md` for one-off project issues - that contaminates every session everywhere.
