//! Repo-local onboarding helpers for newly registered projects.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

const MARKER: &str = "<!-- auwsx:knowledge-collections -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentsSetupOutcome {
    SkippedMissingRepo,
    SkippedNonGitRepo,
    AlreadyPresent,
    Written,
}

pub fn ensure_agents_knowledge_block(
    repo_path: &Path,
    project_name: &str,
) -> Result<AgentsSetupOutcome> {
    if !repo_path.is_dir() {
        return Ok(AgentsSetupOutcome::SkippedMissingRepo);
    }
    if !is_git_repo(repo_path) {
        return Ok(AgentsSetupOutcome::SkippedNonGitRepo);
    }

    let agents_path = repo_path.join("AGENTS.md");
    let existing = match fs::read_to_string(&agents_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| format!("read {}", agents_path.display()));
        }
    };

    if existing.contains(MARKER) {
        return Ok(AgentsSetupOutcome::AlreadyPresent);
    }

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str(&knowledge_collections_block(repo_path, project_name));

    fs::write(&agents_path, next).with_context(|| format!("write {}", agents_path.display()))?;
    Ok(AgentsSetupOutcome::Written)
}

fn is_git_repo(repo_path: &Path) -> bool {
    let git = repo_path.join(".git");
    git.is_dir() || git.is_file()
}

fn knowledge_collections_block(repo_path: &Path, project_name: &str) -> String {
    let language_collections = detect_language_collections(repo_path);
    let knowledge_domains = detect_knowledge_domains(repo_path);
    format!(
        "## auwsx Knowledge Collections\n\
         {MARKER}\n\
         Agents should use these collection hints before answering project/domain questions or changing architecture:\n\
         - coding_language: {}\n\
         - project_domain: {}\n\
         - knowledge_domains: {}\n\
         - update_when: project stack, domain, or knowledge layout changes\n",
        language_collections.join(", "),
        normalize_collection_name(project_name),
        knowledge_domains.join(", ")
    )
}

fn detect_language_collections(repo_path: &Path) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (file, collection) in [
        ("Cargo.toml", "rust"),
        ("package.json", "typescript"),
        ("pyproject.toml", "python"),
        ("go.mod", "go"),
        ("Package.swift", "swift"),
    ] {
        if repo_path.join(file).is_file() {
            out.push(collection);
        }
    }
    if out.is_empty() {
        out.push("project");
    }
    out
}

fn detect_knowledge_domains(repo_path: &Path) -> Vec<&'static str> {
    let knowledge = repo_path.join("knowledge");
    let mut out = Vec::new();
    for domain in ["coding", "domain", "history"] {
        if knowledge.join(domain).is_dir() {
            out.push(domain);
        }
    }
    if out.is_empty() {
        out.extend(["coding", "domain"]);
    }
    out
}

fn normalize_collection_name(name: &str) -> String {
    let mut normalized = String::new();
    let mut last_dash = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_dash = false;
        } else if !last_dash && !normalized.is_empty() {
            normalized.push('-');
            last_dash = true;
        }
    }
    while normalized.ends_with('-') {
        normalized.pop();
    }
    if normalized.is_empty() {
        "project".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::{ensure_agents_knowledge_block, AgentsSetupOutcome, MARKER};
    use std::fs;

    #[test]
    fn given_missing_repo_when_setup_agents_then_skips_without_creating_parent(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let missing = tmp.path().join("missing");

        let outcome = ensure_agents_knowledge_block(&missing, "demo")?;

        assert_eq!(outcome, AgentsSetupOutcome::SkippedMissingRepo);
        assert!(!missing.exists());
        Ok(())
    }

    #[test]
    fn given_existing_non_git_dir_when_setup_agents_then_skips_without_writing(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;

        let outcome = ensure_agents_knowledge_block(tmp.path(), "demo")?;

        assert_eq!(outcome, AgentsSetupOutcome::SkippedNonGitRepo);
        assert!(!tmp.path().join("AGENTS.md").exists());
        Ok(())
    }

    #[test]
    fn given_repo_without_agents_when_setup_agents_then_writes_detected_collections(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        fs::create_dir(tmp.path().join(".git"))?;
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )?;
        fs::create_dir_all(tmp.path().join("knowledge").join("coding"))?;
        fs::create_dir_all(tmp.path().join("knowledge").join("domain"))?;

        let outcome = ensure_agents_knowledge_block(tmp.path(), "Demo Project")?;

        let agents = fs::read_to_string(tmp.path().join("AGENTS.md"))?;
        assert_eq!(outcome, AgentsSetupOutcome::Written);
        assert!(agents.contains(MARKER));
        assert!(agents.contains("- coding_language: rust"));
        assert!(agents.contains("- project_domain: demo-project"));
        assert!(agents.contains("- knowledge_domains: coding, domain"));
        Ok(())
    }

    #[test]
    fn given_existing_agents_when_setup_agents_then_appends_once() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        fs::create_dir(tmp.path().join(".git"))?;
        fs::write(tmp.path().join("AGENTS.md"), "# Existing\n")?;

        assert_eq!(
            ensure_agents_knowledge_block(tmp.path(), "demo")?,
            AgentsSetupOutcome::Written
        );
        assert_eq!(
            ensure_agents_knowledge_block(tmp.path(), "demo")?,
            AgentsSetupOutcome::AlreadyPresent
        );

        let agents = fs::read_to_string(tmp.path().join("AGENTS.md"))?;
        assert_eq!(agents.matches(MARKER).count(), 1);
        assert!(agents.starts_with("# Existing\n\n## auwsx Knowledge Collections"));
        Ok(())
    }
}
