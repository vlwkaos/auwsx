//! Skills bundling. Plan Step 5.
//!
//! On first run / explicit install:
//!   1. For each skill in `skills/` (bundle), check `~/.claude/skills/{name}/`.
//!   2. If missing → copy from bundle.
//!   3. If present (even differing) → leave user's version alone.
//!
//! For non-Claude agents that lack a skill loader, `inline_for_agent(name)`
//! returns the SKILL.md body as raw text to be substituted into the prompt.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const BUNDLED_SKILLS: &[&str] = &[
    "recall",
    "seek",
    "backpressure",
    "write-test",
    "write-test-audit",
    "simplify",
    "good-to-go",
    "commit",
    "no-repeat",
    "memo",
    "memory-retrieve",
    "memory-save",
    "memory-consolidate",
    "note",
    "dream",
    "docs-as-code",
    "deepsleep",
    "sec",
    "keep-my-secret",
    "codex",
    "gh-pr",
];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub installed: Vec<String>,
    pub skipped_existing: Vec<String>,
}

pub fn bundled_skills_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("core crate lives under crates/auwsx-core")
        .join("skills")
}

pub fn install_skills_if_missing(bundle_dir: &Path, target_dir: &Path) -> Result<InstallReport> {
    let mut report = InstallReport::default();

    for skill in BUNDLED_SKILLS {
        let source = bundle_dir.join(skill);
        let target = target_dir.join(skill);

        if target.exists() {
            report.skipped_existing.push((*skill).to_string());
            continue;
        }

        copy_dir_recursive(&source, &target)
            .with_context(|| format!("install bundled skill {skill:?}"))?;
        report.installed.push((*skill).to_string());
    }

    Ok(report)
}

pub fn inline_for_agent(skill: &str) -> Result<String> {
    inline_for_agent_from(&bundled_skills_dir(), skill)
}

pub fn inline_for_agent_from(bundle_dir: &Path, skill: &str) -> Result<String> {
    ensure_bundled(skill)?;
    let skill_md = bundle_dir.join(skill).join("SKILL.md");
    fs::read_to_string(&skill_md)
        .with_context(|| format!("read bundled skill body {}", skill_md.display()))
}

fn ensure_bundled(skill: &str) -> Result<()> {
    if BUNDLED_SKILLS.contains(&skill) {
        Ok(())
    } else {
        bail!("skill {skill:?} is not bundled")
    }
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    if !source.is_dir() {
        bail!("bundled skill source is missing: {}", source.display());
    }

    fs::create_dir_all(target).with_context(|| format!("create {}", target.display()))?;

    for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry.with_context(|| format!("read entry in {}", source.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("read type for {}", entry.path().display()))?;
        let entry_target = target.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &entry_target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &entry_target)
                .with_context(|| format!("copy {}", entry.path().display()))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        bundled_skills_dir, inline_for_agent, inline_for_agent_from, install_skills_if_missing,
        BUNDLED_SKILLS,
    };
    use std::fs;

    #[test]
    fn bundled_skills_have_packaged_skill_files() {
        for skill in BUNDLED_SKILLS {
            let skill_md = bundled_skills_dir().join(skill).join("SKILL.md");
            assert!(
                skill_md.is_file(),
                "BUNDLED_SKILLS entry {skill:?} must have {}",
                skill_md.display()
            );
        }
    }

    #[test]
    fn install_skills_copies_nested_files_and_preserves_existing_targets() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let bundle = tmp.path().join("bundle");
        let target = tmp.path().join("target");

        for skill in BUNDLED_SKILLS {
            let skill_dir = bundle.join(skill);
            fs::create_dir_all(skill_dir.join("scripts"))?;
            fs::write(skill_dir.join("SKILL.md"), format!("# {skill}\n"))?;
            fs::write(skill_dir.join("scripts").join("run.sh"), "echo ok\n")?;
        }

        fs::create_dir_all(target.join("recall"))?;
        fs::write(target.join("recall").join("SKILL.md"), "user copy\n")?;

        let report = install_skills_if_missing(&bundle, &target)?;

        assert!(report.skipped_existing.contains(&"recall".to_string()));
        assert_eq!(
            fs::read_to_string(target.join("recall").join("SKILL.md"))?,
            "user copy\n"
        );
        assert!(target
            .join("codex")
            .join("scripts")
            .join("run.sh")
            .is_file());
        assert_eq!(report.installed.len(), BUNDLED_SKILLS.len() - 1);

        Ok(())
    }

    #[test]
    fn inline_for_agent_reads_only_bundled_skill_bodies() -> anyhow::Result<()> {
        let body = inline_for_agent("good-to-go")?;
        assert!(body.contains("Recurring maintainer audit"));

        let tmp = tempfile::tempdir()?;
        assert!(inline_for_agent_from(tmp.path(), "not-bundled").is_err());

        Ok(())
    }
}
