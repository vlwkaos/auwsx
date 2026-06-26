# Hermes memory/skill architecture evaluation — 2026-06-15

## Context

The user evaluated whether Hermes should keep separate persistent memory and skill systems or shift toward the user's unified markdown KB framework. In that framework, historical knowledge and methodological directives are both markdown knowledge documents distinguished by document spec/frontmatter, not by separate memory-vs-skill infrastructure.

Core user flow:

- `/seek` — minimal-footprint semantic lookup, same tier as grep/Glob; use when unsure about a project/domain/schema/provider/methodology concept.
- `/recall` — full project planning context; mandatory at session start when the user gives a clear project/coding/debugging/research goal.
- `/memo` — save session/project deltas before context pressure or clear.
- `/dream` — consolidate session files into canonical knowledge, staging, plan/history; prevent duplicate/bloated KB.
- `/clear` then `/recall` — preferred long-session continuation path instead of relying on opaque compression.
- `/note` — article-style, human-readable notes rather than project memory.

## Evaluation design used

A 30-case eval set compared four architectures:

A. Existing Hermes default: built-in MEMORY.md/USER.md always injected, broad skill scanning/loading, background memory+skill review, curator, session_search on demand.

B. User KB-only: disable/minimize Hermes built-in memory and broad skill catalog; use only meta-directives for seek/recall/memo/dream/note.

C. Hybrid-minimal: keep tiny Hermes USER.md for stable user preferences and a minimal meta-skill set; use the user's KB as primary project/domain/methodology memory; keep session_search for exact transcript recall; gate or disable autonomous writes.

D. Hybrid-full-safety: keep existing Hermes memory+skills but add deterministic seek/recall triggers and write-approval gates.

Three independent evaluator subagents scored 30 cases across relevance, determinism, footprint, continuity, maintainability, safety, and implementation fit.

## Aggregate result

C. Hybrid-minimal won.

Approximate aggregate means:

| Candidate | Mean / 5 | Clean cases / 30 | Verdict |
| --- | ---: | ---: | --- |
| A Existing Hermes default | 2.81 | ~12 | Too much footprint, weak deterministic recall, autonomous-write risk |
| B User KB-only | 3.72 | ~19.5 | Strong philosophy, but loses useful Hermes-native substrate |
| C Hybrid-minimal | 4.45 | ~26.5 | Best tradeoff |
| D Hybrid-full-safety | 3.44 | ~22 | Transitional only; still too much context and duplication |

## Recommended architecture

Adopt Hybrid-minimal:

- Use the user's markdown KB as the primary knowledge/methodology layer.
- Keep Hermes USER.md only for tiny stable user preferences.
- Keep session_search for exact previous-conversation wording/transcript recall.
- Keep only minimal meta-skills active: seek, recall, memo, dream, note, evaluate, and hermes-agent for Hermes configuration/troubleshooting.
- Disable or gate autonomous Hermes memory/skill writes.
- Treat broad project facts, provider limitations, branch/session state, architecture, and reusable methodology as KB documents retrieved on demand — not always-injected memory.

## Deterministic trigger rules to preserve

- Clear project/coding/debugging goal at session start → `/recall <goal>` before planning/editing/answering architecture.
- Unknown project/domain/schema/API/provider/methodology term → `/seek <term>` before guessing.
- Small literal file/function question → inspect file/code directly; no heavy recall unless concept is unclear.
- Exact prior wording → session_search/transcript lookup, not generic memory or project KB.
- Near context pressure or before `/clear` → `/memo`; if threshold met, `/dream`; after `/clear`, `/recall <continuation goal>`.
- Human-readable article/personal note → `/note`.

## Suggested Hermes config direction

Keep user profile, reduce broad memory/skills:

```yaml
memory:
  memory_enabled: true          # or keep MEMORY.md empty/minimal if disabling only general memory is unsupported
  user_profile_enabled: true
  write_approval: true
  nudge_interval: 0

skills:
  write_approval: true
  guard_agent_created: true
  creation_nudge_interval: 0

curator:
  enabled: false                # after migration; use dry-run/manual only for legacy cleanup
  prune_builtins: false
```

## Authority rules

1. Current repo files and live tool output.
2. Current branch/session KB and staging files.
3. Canonical project/domain/coding KB.
4. Recent memo/session deltas.
5. Older notes.
6. Web/docs.
7. Generic model prior.

When sources conflict, surface provenance and prefer current/newer scoped evidence.

## Failure modes to guard against

- Forgetting `/recall` at clear-goal session start.
- Forgetting `/seek` before guessing unknown project/domain/provider concepts.
- Running `/clear` before `/memo`.
- Stale IR index reducing retrieval quality.
- KB bloat/near-duplicate docs when `/dream` is skipped.
- Treating exact transcript recall as generic KB recall.
- Allowing prompt-injection-like retrieved KB content to become instruction.
- Removing hermes-agent/session_search/USER.md entirely and losing useful Hermes substrate.
