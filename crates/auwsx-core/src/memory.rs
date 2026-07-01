//! Provider-neutral memory operations for prompts and routines.
//!
//! auwsx prompts should name `memory-*` skills, not a specific personal stack.
//! These functions are the stable CLI backend those skills call.

use crate::artifacts;
use crate::db::{global_settings, memory_presets, projects, Db};
use crate::Result;
use anyhow::{anyhow, bail, Context};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const MEMORY_READ_LIMIT: usize = 64 * 1024;
const MEMORY_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryOp {
    Retrieve,
    Save,
    Dream,
    Deepsleep,
}

impl MemoryOp {
    fn as_str(self) -> &'static str {
        match self {
            MemoryOp::Retrieve => "retrieve",
            MemoryOp::Save => "save",
            MemoryOp::Dream => "dream",
            MemoryOp::Deepsleep => "deepsleep",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryBackendKind {
    Portable,
    Command,
    AuwsxSkill,
}

pub async fn retrieve(db: &Db, project_id: i64, query: &str) -> Result<String> {
    let context = load_context(db, project_id).await?;
    match context.backend_kind(MemoryOp::Retrieve)? {
        MemoryBackendKind::Portable => portable_retrieve(project_id, query),
        MemoryBackendKind::Command => {
            let cmd = context.command(MemoryOp::Retrieve)?;
            run_memory_command(&context, MemoryOp::Retrieve, cmd, Some(query), None, None)
        }
        MemoryBackendKind::AuwsxSkill => skills_retrieve(&context.project, query),
    }
}

pub async fn save(db: &Db, project_id: i64, kind: &str, content: &str) -> Result<String> {
    let context = load_context(db, project_id).await?;
    match context.backend_kind(MemoryOp::Save)? {
        MemoryBackendKind::Portable => portable_save(project_id, kind, content),
        MemoryBackendKind::Command => {
            let cmd = context.command(MemoryOp::Save)?;
            run_memory_command(
                &context,
                MemoryOp::Save,
                cmd,
                None,
                Some(kind),
                Some(content),
            )
        }
        MemoryBackendKind::AuwsxSkill => skills_save(&context.project, kind, content),
    }
}

pub async fn consolidate(db: &Db, project_id: i64, mode: &str) -> Result<String> {
    let op = match mode.trim() {
        "dream" => MemoryOp::Dream,
        "deepsleep" | "" => MemoryOp::Deepsleep,
        other => bail!("memory consolidate mode must be dream or deepsleep, got {other}"),
    };
    let context = load_context(db, project_id).await?;
    match context.backend_kind(op)? {
        MemoryBackendKind::Portable => portable_consolidate(project_id, op.as_str()),
        MemoryBackendKind::Command => {
            let cmd = context.command(op)?;
            run_memory_command(&context, op, cmd, None, None, None)
        }
        MemoryBackendKind::AuwsxSkill => skills_consolidate(&context.project, op),
    }
}

struct MemoryContext {
    project: projects::Project,
    preset: memory_presets::MemoryPreset,
}

impl MemoryContext {
    fn backend_kind(&self, op: MemoryOp) -> Result<MemoryBackendKind> {
        let raw = match op {
            MemoryOp::Retrieve => &self.preset.retrieve_kind,
            MemoryOp::Save => &self.preset.save_kind,
            MemoryOp::Dream => &self.preset.dream_kind,
            MemoryOp::Deepsleep => &self.preset.deepsleep_kind,
        };
        Ok(match raw.as_str() {
            "portable" => MemoryBackendKind::Portable,
            "command" => MemoryBackendKind::Command,
            "auwsx_skill" => MemoryBackendKind::AuwsxSkill,
            other => bail!("unknown memory backend kind {other:?} for {}", op.as_str()),
        })
    }

    fn command(&self, op: MemoryOp) -> Result<&str> {
        let cmd = match op {
            MemoryOp::Retrieve => self.preset.retrieve_cmd.as_deref(),
            MemoryOp::Save => self.preset.save_cmd.as_deref(),
            MemoryOp::Dream => self.preset.dream_cmd.as_deref(),
            MemoryOp::Deepsleep => self.preset.deepsleep_cmd.as_deref(),
        };
        cmd.filter(|cmd| !cmd.trim().is_empty()).ok_or_else(|| {
            anyhow!(
                "memory preset {} has no {} command",
                self.preset.name,
                op.as_str()
            )
        })
    }
}

async fn load_context(db: &Db, project_id: i64) -> Result<MemoryContext> {
    let pool = db.pool();
    let settings = global_settings::get(pool).await?;
    let preset = memory_presets::get_by_name(pool, &settings.memory_preset_name)
        .await?
        .ok_or_else(|| anyhow!("unknown memory preset {:?}", settings.memory_preset_name))?;
    let project = projects::get(pool, project_id)
        .await?
        .ok_or_else(|| anyhow!("project {project_id} not found"))?;
    Ok(MemoryContext { project, preset })
}

fn portable_memory_file(project_id: i64) -> Result<PathBuf> {
    let dir = artifacts::data_dir()
        .join("memory")
        .join(format!("project-{project_id}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join("memory.md"))
}

fn portable_retrieve(project_id: i64, query: &str) -> Result<String> {
    let path = portable_memory_file(project_id)?;
    if !path.exists() {
        return Ok(format!(
            "portable-markdown memory is empty for project {project_id}."
        ));
    }
    let mut text = String::new();
    std::fs::File::open(&path)
        .with_context(|| format!("opening {}", path.display()))?
        .take(MEMORY_READ_LIMIT as u64)
        .read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    let query_terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.to_ascii_lowercase())
        .filter(|term| term.len() > 2)
        .collect();
    if query_terms.is_empty() {
        return Ok(text);
    }
    let matches: Vec<&str> = text
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            query_terms.iter().any(|term| lower.contains(term))
        })
        .take(80)
        .collect();
    if matches.is_empty() {
        Ok(format!(
            "No portable-markdown memory lines matched query {query:?}.\n\n{}",
            text.lines().take(40).collect::<Vec<_>>().join("\n")
        ))
    } else {
        Ok(matches.join("\n"))
    }
}

fn portable_save(project_id: i64, kind: &str, content: &str) -> Result<String> {
    let path = portable_memory_file(project_id)?;
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let kind = kind.trim().if_empty("note");
    writeln!(file, "\n## {kind}\n\n{}", content.trim())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(format!(
        "saved portable-markdown memory to {}",
        path.display()
    ))
}

fn portable_consolidate(project_id: i64, mode: &str) -> Result<String> {
    let path = portable_memory_file(project_id)?;
    if !path.exists() {
        return Ok(format!(
            "portable-markdown memory has nothing to consolidate for project {project_id}"
        ));
    }
    Ok(format!(
        "portable-markdown consolidate({mode}) checked {}. No source files were modified.",
        path.display()
    ))
}

fn skills_root(project: &projects::Project) -> PathBuf {
    project
        .skill_path
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| std::env::var("AUWSX_SKILL_PATH").ok().map(PathBuf::from))
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| Path::new(&home).join(".claude").join("skills"))
        })
        .unwrap_or_else(|| PathBuf::from("skills"))
}

fn skills_retrieve(project: &projects::Project, query: &str) -> Result<String> {
    let script = skills_root(project)
        .join("seek")
        .join("scripts")
        .join("seek.sh");
    ensure_script(&script)?;
    let mut command = Command::new("bash");
    command
        .arg(&script)
        .arg(query)
        .current_dir(&project.repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_command_with_timeout(command, MEMORY_COMMAND_TIMEOUT)
        .with_context(|| format!("running {}", script.display()))?;
    if !output.status.success() {
        bail!(
            "auwsx-skills retrieve failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn skills_save(project: &projects::Project, kind: &str, content: &str) -> Result<String> {
    let root = skills_root(project);
    ensure_script(&root.join("memo").join("scripts").join("memo.sh"))?;
    let knowledge = Path::new(&project.repo_path).join("knowledge");
    if !knowledge.is_dir() {
        let receipt = portable_save(project.id, kind, content)?;
        return Ok(format!(
            "project has no knowledge/ directory; saved through portable fallback. {receipt}"
        ));
    }

    let session_dir = knowledge.join("sessions").join("auwsx");
    std::fs::create_dir_all(&session_dir)
        .with_context(|| format!("creating {}", session_dir.display()))?;
    let session_path = session_dir.join(format!("session-{}.md", timestamp_seconds()));
    std::fs::write(
        &session_path,
        format!(
            "---\nslug: auwsx-memory-{kind}\nkind: session\ntitle: auwsx memory {kind}\ndescription: Durable memory captured through auwsx.\nkeywords: [auwsx, memory, {kind}, session, durable, pipeline]\ntarget_slugs: []\n---\n\n# auwsx memory {kind}\n\n{content}\n"
        ),
    )
    .with_context(|| format!("writing {}", session_path.display()))?;
    Ok(format!(
        "saved auwsx-skills session memory to {}. Run memory consolidate --mode dream to promote it.",
        session_path.display()
    ))
}

fn skills_consolidate(project: &projects::Project, op: MemoryOp) -> Result<String> {
    let root = skills_root(project);
    match op {
        MemoryOp::Dream => skills_dream(project, &root),
        MemoryOp::Deepsleep => skills_deepsleep(project, &root),
        MemoryOp::Retrieve | MemoryOp::Save => unreachable!("not a consolidation op"),
    }
}

fn skills_dream(project: &projects::Project, root: &Path) -> Result<String> {
    let script = root.join("memo").join("scripts").join("memo.sh");
    ensure_script(&script)?;
    let mut command = Command::new("bash");
    command
        .arg(&script)
        .arg("detect")
        .current_dir(&project.repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_command_with_timeout(command, MEMORY_COMMAND_TIMEOUT)
        .with_context(|| format!("running {}", script.display()))?;
    if !output.status.success() {
        bail!(
            "auwsx-skills dream detect failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let detected = String::from_utf8_lossy(&output.stdout);
    Ok(format!(
        "auwsx-skills dream setup is present.\n{detected}\nRun the memory-consolidate skill with mode dream for the LLM consolidation pass."
    ))
}

fn skills_deepsleep(project: &projects::Project, root: &Path) -> Result<String> {
    let script = root
        .join("deepsleep")
        .join("scripts")
        .join("deepsleep-audit.sh");
    ensure_script(&script)?;
    let knowledge = Path::new(&project.repo_path).join("knowledge");
    if !knowledge.is_dir() {
        bail!("deepsleep requires a project knowledge/ directory");
    }
    let mut command = Command::new("bash");
    command
        .arg(&script)
        .arg(&knowledge)
        .current_dir(&project.repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_command_with_timeout(command, MEMORY_COMMAND_TIMEOUT)
        .with_context(|| format!("running {}", script.display()))?;
    if !output.status.success() {
        bail!(
            "auwsx-skills deepsleep audit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn ensure_script(path: &Path) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        bail!(
            "memory provider setup missing required script: {}",
            path.display()
        )
    }
}

trait IfEmpty {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str;
}

impl IfEmpty for str {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.is_empty() {
            fallback
        } else {
            self
        }
    }
}

fn run_memory_command(
    context: &MemoryContext,
    op: MemoryOp,
    cmd_template: &str,
    query: Option<&str>,
    kind: Option<&str>,
    content: Option<&str>,
) -> Result<String> {
    if cmd_template.contains("{content}") {
        bail!("memory command templates must use {{content_file}}, not {{content}}");
    }
    let content_file = if let Some(content) = content {
        Some(write_command_content_file(context.project.id, op, content)?)
    } else {
        None
    };
    let skill_root = skills_root(&context.project);
    let memory_dir = portable_memory_file(context.project.id)?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| artifacts::data_dir().join("memory"));
    let (template_argv, stdin) = crate::agent::build_argv(cmd_template, "")?;
    let project_id = context.project.id.to_string();
    let project_root = context.project.repo_path.as_str();
    let skill_root = skill_root.to_string_lossy();
    let memory_dir = memory_dir.to_string_lossy();
    let content_file = content_file
        .as_ref()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    let argv = template_argv
        .into_iter()
        .map(|arg| {
            arg.replace("{query}", query.unwrap_or_default())
                .replace("{kind}", kind.unwrap_or_default())
                .replace("{mode}", op.as_str())
                .replace("{project_id}", &project_id)
                .replace("{project_root}", project_root)
                .replace("{skill_root}", &skill_root)
                .replace("{memory_dir}", &memory_dir)
                .replace("{content_file}", &content_file)
        })
        .collect::<Vec<_>>();
    let Some((program, args)) = argv.split_first() else {
        bail!("empty memory command for {}", op.as_str());
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&context.project.repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin {
        command.stdin(Stdio::piped());
    }
    let output = run_command_with_timeout(command, MEMORY_COMMAND_TIMEOUT)
        .with_context(|| format!("running memory {} command", op.as_str()))?;
    if !output.status.success() {
        bail!(
            "memory {} command failed: {}",
            op.as_str(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn write_command_content_file(project_id: i64, op: MemoryOp, content: &str) -> Result<PathBuf> {
    let dir = artifacts::data_dir()
        .join("memory")
        .join(format!("project-{project_id}"))
        .join("command-input");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}-{}.txt", op.as_str(), timestamp_seconds()));
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn run_command_with_timeout(mut command: Command, timeout: Duration) -> Result<Output> {
    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map_err(Into::into);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            bail!(
                "command timed out after {}s; stderr: {}",
                timeout.as_secs(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn timestamp_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::codex;
    use crate::db::projects::{self, NewProject};
    use crate::db::Db;
    use crate::db::MemoryPreset;
    use tokio::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    async fn project(db: &Db) -> anyhow::Result<i64> {
        projects::create(
            db.pool(),
            NewProject {
                name: "demo",
                repo_path: ".",
                default_branch: "main",
                arsenal_preset_name: None,
                main_agent_cmd: codex::DEFAULT_CMD,
                route_agent_cmd: codex::DEFAULT_CMD,
                plan_agent_cmd: codex::DEFAULT_CMD,
                work_agent_cmd: codex::DEFAULT_CMD,
                review_agent_cmd: None,
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
                schedule_cron: None,
            },
            1,
        )
        .await
    }

    #[tokio::test]
    async fn given_portable_markdown_when_saved_then_retrieve_finds_matching_line(
    ) -> anyhow::Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let settings = global_settings::get(db.pool()).await?;
        global_settings::update(
            db.pool(),
            "portable-markdown",
            &settings.pipeline_ux_guidance,
            2,
        )
        .await?;
        let project_id = project(&db).await?;

        save(&db, project_id, "result", "shipped memory interface").await?;
        let got = retrieve(&db, project_id, "memory").await?;

        assert!(got.contains("shipped memory interface"));
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_empty_portable_markdown_when_consolidated_then_reports_no_source_write(
    ) -> anyhow::Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());

        let got = portable_consolidate(9_876_543_210, "deepsleep")?;

        assert!(got.contains("nothing to consolidate"));
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[test]
    fn given_command_placeholders_when_run_then_dynamic_values_stay_single_args(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path().join("data"));
        let repo = tmp.path().join("repo with spaces");
        let scripts = repo.join("scripts");
        std::fs::create_dir_all(&scripts)?;
        let script = scripts.join("memory.sh");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env bash
printf 'argc=%s\nquery=%s\ncontent_file=%s\n' "$#" "$1" "$2"
"#,
        )?;
        let context = MemoryContext {
            project: projects::Project {
                id: 42,
                profile_id: 1,
                profile_order: 0,
                name: "demo".to_string(),
                repo_path: repo.to_string_lossy().to_string(),
                default_branch: "main".to_string(),
                arsenal_preset_name: None,
                main_agent_cmd: codex::DEFAULT_CMD.to_string(),
                route_agent_cmd: codex::DEFAULT_CMD.to_string(),
                plan_agent_cmd: codex::DEFAULT_CMD.to_string(),
                work_agent_cmd: codex::DEFAULT_CMD.to_string(),
                review_agent_cmd: None,
                main_agent_cmd_override: None,
                route_agent_cmd_override: None,
                plan_agent_cmd_override: None,
                work_agent_cmd_override: None,
                review_agent_cmd_override: None,
                skill_path: None,
                merge_mode: projects::MergeMode::Local,
                completion_policy: projects::CompletionPolicy::Manual,
                plan_gate_timeout_min: 10,
                completion_soft_timeout_min: 60,
                schedule_cron: None,
                max_concurrency: 3,
                iteration_timeout_min: 30,
                main_job_timeout_min: 60,
                review_max_rounds: 5,
                conflict_max_attempts: 3,
                deepsleep_cron: None,
                last_deepsleep_at: None,
                created_at: 1,
            },
            preset: MemoryPreset {
                id: 1,
                name: "cmd".to_string(),
                retrieve_kind: "command".to_string(),
                retrieve_cmd: None,
                save_kind: "command".to_string(),
                save_cmd: None,
                dream_kind: "command".to_string(),
                dream_cmd: None,
                deepsleep_kind: "command".to_string(),
                deepsleep_cmd: None,
                builtin: false,
                created_at: 1,
                updated_at: 1,
            },
        };

        let got = run_memory_command(
            &context,
            MemoryOp::Save,
            "bash {project_root}/scripts/memory.sh {query} {content_file}",
            Some("alpha beta --flag"),
            None,
            Some("saved content"),
        )?;

        assert!(got.contains("argc=2"));
        assert!(got.contains("query=alpha beta --flag"));
        assert!(got.contains("content_file="));
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[test]
    fn given_command_template_with_raw_content_when_run_then_rejected() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path().join("data"));
        let context = MemoryContext {
            project: projects::Project {
                id: 43,
                profile_id: 1,
                profile_order: 0,
                name: "demo".to_string(),
                repo_path: ".".to_string(),
                default_branch: "main".to_string(),
                arsenal_preset_name: None,
                main_agent_cmd: codex::DEFAULT_CMD.to_string(),
                route_agent_cmd: codex::DEFAULT_CMD.to_string(),
                plan_agent_cmd: codex::DEFAULT_CMD.to_string(),
                work_agent_cmd: codex::DEFAULT_CMD.to_string(),
                review_agent_cmd: None,
                main_agent_cmd_override: None,
                route_agent_cmd_override: None,
                plan_agent_cmd_override: None,
                work_agent_cmd_override: None,
                review_agent_cmd_override: None,
                skill_path: None,
                merge_mode: projects::MergeMode::Local,
                completion_policy: projects::CompletionPolicy::Manual,
                plan_gate_timeout_min: 10,
                completion_soft_timeout_min: 60,
                schedule_cron: None,
                max_concurrency: 3,
                iteration_timeout_min: 30,
                main_job_timeout_min: 60,
                review_max_rounds: 5,
                conflict_max_attempts: 3,
                deepsleep_cron: None,
                last_deepsleep_at: None,
                created_at: 1,
            },
            preset: MemoryPreset {
                id: 1,
                name: "cmd".to_string(),
                retrieve_kind: "command".to_string(),
                retrieve_cmd: None,
                save_kind: "command".to_string(),
                save_cmd: None,
                dream_kind: "command".to_string(),
                dream_cmd: None,
                deepsleep_kind: "command".to_string(),
                deepsleep_cmd: None,
                builtin: false,
                created_at: 1,
                updated_at: 1,
            },
        };

        let err = run_memory_command(
            &context,
            MemoryOp::Save,
            "echo {content}",
            None,
            None,
            Some("raw content"),
        )
        .expect_err("raw content placeholder must be rejected");

        assert!(err.to_string().contains("{content_file}"));
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[test]
    fn given_command_timeout_when_run_then_process_is_killed() -> anyhow::Result<()> {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 1");

        let err = run_command_with_timeout(command, Duration::from_millis(10))
            .expect_err("sleep should time out");

        assert!(err.to_string().contains("timed out"));
        Ok(())
    }
}
