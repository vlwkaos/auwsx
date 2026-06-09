---
slug: good-to-go-axes
kind: coding
---

# auwsx good-to-go axes

Project-specific audit axes, extending the skill defaults. Append on each run.

## Enum ↔ SQL CHECK domain parity

Every Rust enum that serializes into a `TEXT` column constrained by a SQL
`CHECK (col IN (...))` must match that domain **exactly, both directions** — a
variant with no CHECK value is a write that fails only at runtime; a CHECK value
with no variant is a row the loader can't parse.

Pairs to keep in lockstep (`src/db/migrations/0001_init.sql` is source of truth):

| Rust | SQL column |
|------|-----------|
| `state::IssueStatus` (SCREAMING_SNAKE) | `issues.status` |
| `main_jobs::MainJobStatus` (SCREAMING_SNAKE) | `main_jobs.status` |
| `backlog::Source` (snake) | `backlog_items.source` |
| `backlog::Approval` (snake) | `backlog_items.approval` |
| `steering::SteeringSource` (snake) | `steering.source` |
| `db::projects::MergeMode` (snake) | `projects.merge_mode` |
| `db::projects::CompletionPolicy` (snake) | `projects.completion_policy` |
| `db::findings::Severity` (snake) | `findings.severity` |
| `db::findings::FindingStatus` (snake) | `findings.status` |

These enums use hand-rolled `as_str`/`from_str` (NOT serde) for the SQL bind,
mirroring `IssueStatus`. `tests/crud.rs` proves parity at runtime with a
positive-control insert per enum (a valid `as_str()` value must pass the CHECK)
plus a `from_str(as_str()) == Some(v)` round-trip — so a drift fails a test, not
just the diff below.

Check:
```bash
rg -oN '=> "[A-Z_]+"' crates/auwsx-core/src/state.rs | rg -o '[A-Z_]+' | sort -u > "$TMPDIR/enum.txt"
sed -n '/status TEXT NOT NULL CHECK (status IN (/,/))/p' crates/auwsx-core/src/db/migrations/0001_init.sql \
  | rg -o "'[A-Z_]+'" | tr -d "'" | sort -u > "$TMPDIR/sql.txt"
comm -3 "$TMPDIR/enum.txt" "$TMPDIR/sql.txt"   # any output = drift = FAIL
```
The `state.rs` test `given_each_variant_when_as_str_then_matches_spec_id` plus
the `tests/db_smoke.rs` CHECK-rejection cases guard this, but they encode the
domain by hand — re-run the diff above whenever either side changes.

## Module-rename doc sync

`CLAUDE.md` is a symlink to `AGENTS.md` (one file). When a `crates/.../src`
module is renamed (e.g. `drafts.rs`→`backlog.rs`, `followups.rs`→`steering.rs`),
update the `## Key Files` list in `AGENTS.md` — both the path and its one-line
description. The design plan (`~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md`)
is the architecture PRD; it lags the code intentionally during a model pivot and
is synced as its own task, not gated here.
