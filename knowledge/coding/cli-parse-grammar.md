---
slug: cli-parse-grammar
kind: coding
title: auwsx hand-rolled CLI parse grammar + Parsed helper
description: Conventions for the no-clap hand-rolled `parse`/`Parsed` CLI layer — flag grammar, opt_int optional-int contract, enum-flag idiom, and the Parsed::new behavior gotchas pinned by tests.
keywords: [parse, Parsed, opt_int, hand-rolled CLI, no clap, flag grammar, last-wins, equals-form, CliAction, arsenal, project add, completion_policy flag, from_str lowercase, i64 overflow]
created: 2026-06-09
modified: 2026-06-17
---

# auwsx hand-rolled CLI parse grammar

The `auwsx` CLI (`auwsx-tui/src/cli.rs`) hand-rolls its argument parsing — **no
clap**. A pure `parse(args) -> Result<CliAction>` builds a `Parsed` token map,
then each subcommand reads typed values off it. Keeps parsing deterministic and
unit-testable in-file (`#[cfg(test)]`, since `auwsx-tui` is bin-only).

## Parsed token map

`Parsed::new(args)` walks the argv into a `HashMap<String, Option<String>>`:

- `--key value` ⇒ `key → Some("value")`.
- `--key=value` equals-form ⇒ split on first `=` ⇒ `key → Some("value")`.
- `--key` with NO following value (end of args, or next token starts with `--`)
  ⇒ `key → None` (treated as a **boolean flag**, not an error).
- Repeated `--key a --key b` ⇒ HashMap insert overwrites ⇒ **last-wins** (`b`).

## Typed accessors

| Accessor | Contract |
|----------|----------|
| `flag(key) -> Option<String>` | raw string value, or `None` if absent / bare |
| `int(key) -> Result<i64>` | positional/required int; missing or unparseable ⇒ `Err` |
| `opt_int(key) -> Result<Option<i64>>` | **optional int**: absent ⇒ `Ok(None)`; present-but-not-an-integer ⇒ `Err` (rejected, NOT silently dropped). Fills the gap `int` left for optional flags. |

### Enum-flag idiom

Reused verbatim across `--source` / `--approval` / `--status` /
`--completion-policy`:

```rust
match p.flag("completion-policy") {
    Some(s) => Some(CompletionPolicy::from_str(&s).with_context(...)?),
    None => None,
}
```

`from_str` is **exact lowercase**, matching the SQL CHECK domain (see
coding/db-crud-conventions.md enum split).

## `project add` policy flags

Three optional flags map onto the `NewProject` overrides (see
create-override-coalesce-default in db-crud-conventions.md):

| Flag | Field | Type |
|------|-------|------|
| `--completion-policy <manual\|soft\|auto>` | `completion_policy` | enum |
| `--plan-gate-timeout <int>` | `plan_gate_timeout_min` | `opt_int` |
| `--completion-timeout <int>` | `completion_soft_timeout_min` | `opt_int` |

All optional → `None` when absent (DB DEFAULT kept). `print_response`'s Project
arm prints a `policy:` line (completion / plan_gate / completion_soft /
concurrency) so `project get` / `ls` verify an override actually took.

## `arsenal` global preset grammar

Global Arsenal presets are daemon-owned convenience rows for reusable per-role
agent command templates. The CLI parses these into IPC commands:

| Command | IPC |
|---------|-----|
| `arsenal ls` / `arsenal list` | `Command::ListArsenalPresets` |
| `arsenal set <name> --main <cmd> --plan <cmd> --work <cmd> [--review <cmd>]` | `Command::UpsertArsenalPreset` |

`name`, `--main`, `--plan`, and `--work` are required at parse time. `--review`
is optional and maps to `None` when absent. Blank string validation belongs to
the DB/IPC boundary (`db::arsenal::upsert`), not to `parse`.

## Behavior gotchas (pinned by tests)

| Input | Result | Why |
|-------|--------|-----|
| bare `--plan-gate-timeout` (end of args) | `None`, **not Err** | bare flag ⇒ boolean ⇒ `opt_int` reads absent |
| `--plan-gate-timeout -5` | `Some(-5)` | `-5` doesn't start with `--` ⇒ taken as value; `i64` accepts it |
| negative timeout | passes through unvalidated at parse AND DB layer | no sign validation, no CHECK on timeout columns; negative gate behaves like 0 (always due) |
| `--completion-policy auto --completion-policy soft` | `Some(Soft)` | last-wins (HashMap overwrite) |
| `--completion-policy=auto` / `--plan-gate-timeout=5` | parsed | equals-form (split_once('=')) for enum AND int flags |
| `--completion-policy AUTO` | `Err` | `from_str` exact-lowercase |
| `--completion-policy " "` (whitespace) | `Err` | not a valid lowercase variant |
| `--completion-policy=` (empty) | `Err` | empty string ⇒ no variant match |
| `--plan-gate-timeout 9223372036854775808` (i64 overflow) | `Err` | `i64::from_str` overflow |
