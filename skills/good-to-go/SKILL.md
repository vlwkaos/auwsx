---
name: good-to-go
description: "Recurring maintainer audit: doc sync, internal consistency (unexposed types/variants), test coverage devil's advocate, build check. Run after any feature addition or before release."
allowed-tools: Bash, Read, Edit, Write, Glob, Grep, Agent
argument-hint: "[scope: commit|branch|all]"
---

# Good to Go

Recurring maintainer audit. Treats checks as a growing factory of axes — discovers new ones each run and persists them. Not a one-off pre-release gate.

## Step 0 — Gather Data

```bash
bash ~/.claude/skills/good-to-go/scripts/gather.sh "${ARGUMENTS:-default}"
```

Interpret: scope of changes, recurring release patterns from git history, doc file inventory, CHANGELOG state, build + test results.

## Step 1 — Learn Audit Axes

Load project-specific axes from `knowledge/coding/good-to-go-axes.md` if it exists — a direct `Read` of that fixed path (it is a convention; the skill knows the target, so no `/seek` or `/recall` is needed). If the project has no `knowledge/` tree, fall back to the `AGENTS.md` `## good-to-go` section. These extend the default axes below — do not replace them.

Default axes (apply to every run):

| Axis | What to check |
|------|---------------|
| **Docs** | All language variants (README.ko, etc.) reflect changes in primary |
| **CHANGELOG** | Unreleased entry covers current changes; breaking changes explicit |
| **Tests** | New/changed functions have coverage; devil's advocate for edge cases |
| **Build** | Type-check, test, and lint all pass — discover available commands from project manifest (package.json scripts, Cargo.toml, Makefile); run type-check separately from tests |
| **Consistency** | (see Step 4) |
| **Subprocess protocol** | When a spawned subprocess command changes, verify the replacement maintains the I/O contract (see Step 4) |
| **Shell portability** | Embedded scripts and generated shell commands: shebangs use `#!/usr/bin/env bash` not `#!/bin/bash`; runtime paths use `$HOME` not absolute user paths baked at code/save time |

## Step 2 — Docs Audit

For each language variant pair (e.g. `README.md` + `README.ko.md`):
- Was the primary changed in this diff but the variant not touched? → FAIL
- Is a feature section present in primary but missing from variant? → FAIL

## Step 3 — Test Coverage (Devil's Advocate)

Get source diff (exclude test files and docs):

```bash
git diff --unified=0 HEAD~1..HEAD -- $(git diff --name-only HEAD~1..HEAD | grep -v test | grep -v spec | grep -v '\.md$')
```

For each changed function, ask:
1. What input panics or returns wrong value?
2. What boundary is off-by-one?
3. What env/filesystem condition makes this flake?
4. Is the error path tested?

List untested scenarios with minimal pseudocode. Do NOT skip even if tests pass.

## Step 4 — Internal Consistency Audit

**The axis most often skipped.** Catches: enum variants defined but never constructed outside tests; public functions never called; CLI/MCP surfaces that don't expose all type-level options.

**Rust:**
```bash
# Find enum variants — check each is constructed somewhere in non-test src
rg 'pub enum \w+' src/ --include='*.rs' -l

# For each enum found, list its variants and grep for construction sites
# e.g. for Verbosity: rg 'Verbosity::Quiet' src/ --include='*.rs' -l

# CLI flags vs enum variants: compare #[arg(long)] entries against enum variants
rg '#\[arg\(long' src/cli/ --include='*.rs'
```

**What to flag:**
- Variant `Foo::Bar` exists in types but never constructed in non-test code → WARN (likely unexposed)
- Variant `Foo::Bar` exists but no `--bar` CLI arg and no MCP field → FAIL if it's user-facing behavior
- Public `fn` with no callers outside its own module → WARN
- Env var `IR_*` referenced in docs/comments but never `std::env::var`'d → FAIL

**JS/TS:**
```bash
# Exported symbols with no import sites
rg 'export (function|const|class|type|interface)' src/ -l
```

This step found `Verbosity::Quiet` in `types.rs` not wired to `--quiet` CLI flag (ir v0.8.2). Add catches like this to the knowledge file after each run.

**Shell portability — apply whenever scripts are embedded in source or commands are generated into config files:**

1. Shebangs: `grep -r '^#!/bin/bash' .` — any match → FAIL, must be `#!/usr/bin/env bash`
2. Baked paths: search for absolute user-home paths in generated/embedded strings — `grep -r '/Users/\|/home/' src/` — any match in a string that ends up in a generated file → FAIL, use `$HOME` instead
3. Verify `$HOME` is in a double-quoted context so the shell expands it at runtime, not in single quotes

**External subprocess protocol — apply when any spawned subprocess command changes (new binary, new flags, external tool replacing custom code):**

1. Enumerate edge-case lines likely to produce zero output from the new tool (punctuation-only, whitespace-only, lines whose entire content is in a stop-tag or filter list).
2. Run the probe: `printf '.\n<non-filtered-line>\ntest\n' | <new_command> 2>/dev/null | wc -l` — must equal 3.
3. If count < 3 → binary drops lines → WARN: verify the calling code handles the 0-output case (sentinel protocol or equivalent). If not handled → FAIL.
4. Check: does the test suite include at least one test where `process_line` is called with a line the subprocess would drop? If absent → FAIL.

## Step 5 — Report

```
## Good to Go — Audit

### Docs
- [PASS/FAIL/WARN] ...

### Tests
- [PASS/FAIL/WARN] ...

### Build
- [PASS/FAIL/WARN] ...

### Consistency
- [PASS/FAIL/WARN] ...

### Proposed Missing Tests / Fixes
1. ...
```

## Step 6 — Expand the Factory

Append any new axes discovered this run to `knowledge/coding/good-to-go-axes.md` (create it with front-matter — `slug: good-to-go-axes`, `kind: coding` — if absent; on a project with no `knowledge/` tree, fall back to `AGENTS.md ## good-to-go`). Keep axes in this knowledge file, never in `AGENTS.md`: `AGENTS.md` is loaded into every session, so an ever-growing axis list there is permanent dead weight, whereas the knowledge file is read only when this skill runs. Append directly — do NOT route through `/memo`: on a feature branch `/memo` stages knowledge, so the axis would not land in the canonical file and the next run would not see it. Remove an axis only when the condition it guards is structurally eliminated — enforced by a compiler check, lint rule, or test that cannot be bypassed.

If any FAIL items remain unresolved, save summary via `/memo`.
