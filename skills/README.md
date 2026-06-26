# Bundled skills

This directory contains the skill files auwsx ships so that fresh installs
have the pipeline's referenced skills available even without prior user setup.

**On install:** `auwsx_core::skills::install_skills_if_missing()` copies each
of these into `~/.claude/skills/{name}/` ONLY IF a directory of that name does
not already exist there. User's own version is never overwritten.

**For non-Claude agents:** Codex and opencode lack `/skill` resolution. The
corresponding `AgentRunner` impl substitutes any `/skill-name` mention in the
prompt with the inlined SKILL.md body of the same name from this directory
(via `skills::inline_for_agent`).

## Packaged Skills

| Skill | Required by |
|-------|-------------|
| `recall` | iteration prompt / session context loading |
| `seek` | `recall` semantic lookup |
| `backpressure` | requirement and quality loop |
| `write-test` | `backpressure` test generation loop |
| `write-test-audit` | `backpressure` test audit loop |
| `simplify` | `backpressure` simplification loop |
| `good-to-go` | pre-merge audit / human verification gate |
| `commit` | commit workflow |
| `no-repeat` | worker failure breadcrumb / repeat-mistake prevention |
| `memo` | post-merge and session knowledge propagation |
| `memory-retrieve` | provider-neutral memory lookup contract backed by the selected Memory preset |
| `memory-save` | provider-neutral durable memory save contract backed by the selected Memory preset |
| `memory-consolidate` | provider-neutral dream/deepsleep contract backed by the selected Memory preset |
| `note` | `memo` personal-note handoff |
| `dream` | session consolidation and recurring knowledge maintenance |
| `docs-as-code` | `dream` knowledge-to-code annotation step |
| `deepsleep` | built-in weekly routine and `dream` hygiene audit |
| `sec` | `backpressure` security audit |
| `keep-my-secret` | `sec` secret scanning handoff |
| `codex` | `write-test` independent test-writing command |
| `gh-pr` | PR creation for PR merge mode |

## Packaging Rule

Every skill listed in `auwsx_core::skills::BUNDLED_SKILLS` must exist at
`skills/{name}/SKILL.md`. If a skill calls another skill as part of normal
operation, include that dependency too. Optional external callers do not need to
be bundled just because they can invoke one of these skills.
