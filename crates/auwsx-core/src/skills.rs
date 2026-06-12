//! Skills bundling. Plan Step 5.
//!
//! On first run / explicit install:
//!   1. For each skill in `skills/` (bundle), check `~/.claude/skills/{name}/`.
//!   2. If missing → copy from bundle.
//!   3. If present (even differing) → leave user's version alone.
//!
//! For non-Claude agents that lack a skill loader, `inline_for_agent(name)`
//! returns the SKILL.md body as raw text to be substituted into the prompt.

pub const BUNDLED_SKILLS: &[&str] = &[
    "recall",
    "backpressure",
    "commit",
    "memo",
    "dream",
    "deepsleep",
    "gh-pr",
];

// TODO: install_skills_if_missing(bundle_dir, target_dir) -> Result<InstallReport>
// TODO: inline_for_agent(skill: &str, agent: &str) -> Result<String>
