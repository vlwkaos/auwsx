---
name: docs-as-code
description: Write permanent comments on critical code paths to document implementation context — which files to touch, why, and how they connect. Use when implementing a feature, finishing one, or whenever a code path is non-obvious to future implementors.
argument-hint: "[feature or change]"
---

# Docs-as-Code

Implementation history lives in the code, not in external files.

Two complementary mechanisms:
1. Change-surface comments (`// ^`) — inline in source
2. Knowledge references (`[[slug]]`) — link code to `knowledge/` files

---

## 1. Change Surface Comments

On the critical code path of a feature — the entry point or the file that must be found first — write a permanent `// ^` comment that guides future implementors to all related files.

```rust
// ^ To add an auth strategy: also update config.rs (env vars), db/schema.rs (users table)
```

```ts
// ^ Payment flow entry point. Related: webhook.ts (Stripe events), db/orders.ts (state machine)
```

The comment must answer: "if I need to change this, what else do I need to find?"

If the list grows beyond 3-4 files on the next implementation cycle: the architecture has a coupling problem — raise it.

---

## 2. Knowledge References

When a code path implements or depends on documented knowledge, link it:

```rust
// ^ [[auth-flow]]
pub fn authenticate(token: &str) -> Result<Session> {
```

```ts
// ^ [[order-state-machine]]
export function transitionOrder(order: Order, event: Event) {
```

`[[slug]]` is a permanent reference to `knowledge/domain/{slug}.md` or `knowledge/coding/{slug}.md`.

In vault modes (no `knowledge/` dir), `[[slug]]` serves as a change-surface alert: it marks code paths conceptually linked to vault knowledge at `{vault_path}/domain/{slug}.md` or `{vault_path}/coding/{slug}.md`. Do not remove these comments when reverting from docs-as-code.

### Finding Definition

```bash
# Direct lookup
ls knowledge/**/{slug}.md

# Search by keyword
rg --files knowledge/ | rg -i "{keyword}"
```

### Finding All Callers

```bash
# All code locations that depend on a knowledge concept
rg '\[\[{slug}\]\]'

# All slugs referenced in a specific file
rg '\[\[[a-z][a-z0-9-]*\]\]' src/auth.rs
```

### Change-Surface Analysis

When updating a knowledge file, find all code that may need review:

```bash
rg '\[\[{slug}\]\]'
```

If results span > 5 files: the concept has broad coupling. Flag for architectural review.

---

## Standards

### Comment Syntax

| Language | Syntax |
|----------|--------|
| Rust, TS, Go, JS | `// ^ [[slug]]` or `// ^ description` |
| Python, Shell | `# ^ [[slug]]` or `# ^ description` |
| SQL | `-- ^ [[slug]]` or `-- ^ description` |

### Rules

- Permanent — do not remove unless the architecture changes
- Place on the function/type/module that is the natural entry point, not at the top of the file
- Be specific: name the file AND why it matters (for change-surface comments)
- `[[slug]]` must match an actual filename in `knowledge/` (or a `target_slug` in a session)
- One `[[slug]]` reference per logical entry point — don't duplicate on every related function
