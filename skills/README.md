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

## Skills to ship

| Skill         | Required by                                                                 |
|---------------|------------------------------------------------------------------------------|
| `recall`      | iteration prompt (initial context load)                                      |
| `backpressure`| QA phase after each iteration                                                |
| `commit`      | COMPLETING phase                                                             |
| `memo`        | post_merge knowledge propagation                                             |
| `dream`       | post_merge + built-in routine + manual one-off                               |
| `deepsleep`   | built-in weekly routine                                                      |
| `gh-pr`       | COMPLETING + merge_mode = pr or auto-detect with GitHub remote               |

## Population strategy

For now, copies must be pulled in manually (or symlinked from `~/.claude/skills/`
during dev). A future build step or `auwsx skills sync` subcommand will fetch
them from the user's `~/.claude/skills/` directory at package time.
