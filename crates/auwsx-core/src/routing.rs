//! Backlog routing service.
//!
//! Approved backlog enters the issue pipeline here. The compatibility path
//! still creates one issue per item; the scheduler path uses a cheap route
//! agent to decide whether a backlog item belongs in an existing queue-capable
//! issue.

use crate::agent::{self, AgentExecutor, AgentSpec, ExitKind};
use crate::artifacts;
use crate::backlog::{self, Approval, BacklogItem};
use crate::db::routing_runs::{self, FinishRoutingRun, StartRoutingRun};
use crate::db::{issues, Issue, Project};
use crate::state::IssueStatus;
use crate::steering::SteeringSource;
use crate::Result;
use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

const ROUTE_TIMEOUT_SECS: u64 = 120;
const ROUTE_LOCK_STALE_MS: i64 = 10 * 60 * 1000;
const DUPLICATE_TOKEN_OVERLAP_MIN: usize = 6;
const DUPLICATE_TOKEN_SCORE_MIN: f32 = 0.62;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteOutcome {
    CreatedIssue {
        item_id: i64,
        issue_id: i64,
    },
    AttachedToIssue {
        item_id: i64,
        issue_id: i64,
        message_id: i64,
    },
}

impl RouteOutcome {
    pub fn item_id(&self) -> i64 {
        match self {
            Self::CreatedIssue { item_id, .. } | Self::AttachedToIssue { item_id, .. } => *item_id,
        }
    }

    pub fn issue_id(&self) -> i64 {
        match self {
            Self::CreatedIssue { issue_id, .. } | Self::AttachedToIssue { issue_id, .. } => {
                *issue_id
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::CreatedIssue { .. } => "created",
            Self::AttachedToIssue { .. } => "attached",
        }
    }
}

pub struct RouteDeps<'a> {
    pub pool: &'a SqlitePool,
    pub project: &'a Project,
    pub executor: &'a dyn AgentExecutor,
    pub socket: &'a Path,
    pub now: i64,
}

#[derive(Debug, Clone)]
pub struct RouteOneResult {
    pub outcome: RouteOutcome,
    pub fallback_reason: Option<String>,
}

/// Compatibility/admin path: approve backlog, then create one issue per item.
/// Scheduler code should use [`route_approved_project_semantic`].
pub async fn route_approved_project(
    pool: &SqlitePool,
    project_id: i64,
    now: i64,
) -> Result<Vec<RouteOutcome>> {
    let approved = backlog::list_by_approval(pool, project_id, Approval::Approved).await?;
    let mut routed = Vec::new();
    for item in approved {
        if item.consumed_issue_id.is_some() {
            continue;
        }
        let issue_id = create_issue_from_item(pool, &item, None, None, now).await?;
        routed.push(RouteOutcome::CreatedIssue {
            item_id: item.id,
            issue_id,
        });
    }
    Ok(routed)
}

pub async fn route_approved_project_semantic(deps: &RouteDeps<'_>) -> Result<Vec<RouteOneResult>> {
    if !acquire_project_route_lock(deps.pool, deps.project.id, deps.now).await? {
        return Ok(Vec::new());
    }
    let result = route_approved_project_semantic_locked(deps).await;
    if let Err(e) = release_project_route_lock(deps.pool, deps.project.id).await {
        if result.is_ok() {
            return Err(e);
        }
    }
    result
}

async fn route_approved_project_semantic_locked(
    deps: &RouteDeps<'_>,
) -> Result<Vec<RouteOneResult>> {
    let approved =
        backlog::list_by_approval(deps.pool, deps.project.id, Approval::Approved).await?;
    let mut routed = Vec::new();
    for item in approved {
        if item.consumed_issue_id.is_some() {
            continue;
        }
        routed.push(route_item_semantic(deps, item).await?);
    }
    Ok(routed)
}

pub async fn route_one_now(pool: &SqlitePool, item_id: i64, now: i64) -> Result<i64> {
    let item = backlog::get(pool, item_id)
        .await?
        .ok_or_else(|| anyhow!("backlog item {item_id} not found"))?;
    if item.approval == Approval::Dismissed {
        return Err(anyhow!("backlog item {item_id} is dismissed"));
    }
    if let Some(issue_id) = item.consumed_issue_id {
        return Ok(issue_id);
    }
    if item.approval == Approval::Pending {
        backlog::approve(pool, item_id, now).await?;
    }
    create_issue_from_item(pool, &item, None, None, now).await
}

pub async fn route_one_now_semantic(deps: &RouteDeps<'_>, item_id: i64) -> Result<RouteOneResult> {
    if !acquire_project_route_lock(deps.pool, deps.project.id, deps.now).await? {
        bail!("project {} is already routing backlog", deps.project.id);
    }
    let result = route_one_now_semantic_locked(deps, item_id).await;
    if let Err(e) = release_project_route_lock(deps.pool, deps.project.id).await {
        if result.is_ok() {
            return Err(e);
        }
    }
    result
}

async fn route_one_now_semantic_locked(
    deps: &RouteDeps<'_>,
    item_id: i64,
) -> Result<RouteOneResult> {
    let item = backlog::get(deps.pool, item_id)
        .await?
        .ok_or_else(|| anyhow!("backlog item {item_id} not found"))?;
    if item.approval == Approval::Dismissed {
        bail!("backlog item {item_id} is dismissed");
    }
    if let Some(issue_id) = item.consumed_issue_id {
        return Ok(RouteOneResult {
            outcome: RouteOutcome::CreatedIssue {
                item_id: item.id,
                issue_id,
            },
            fallback_reason: None,
        });
    }
    if item.approval == Approval::Pending {
        backlog::approve(deps.pool, item_id, deps.now).await?;
    }
    route_item_semantic(deps, item).await
}

async fn acquire_project_route_lock(pool: &SqlitePool, project_id: i64, now: i64) -> Result<bool> {
    sqlx::query("DELETE FROM project_route_locks WHERE acquired_at < ?")
        .bind(now.saturating_sub(ROUTE_LOCK_STALE_MS))
        .execute(pool)
        .await?;
    let n = sqlx::query(
        "INSERT OR IGNORE INTO project_route_locks (project_id, acquired_at)
         VALUES (?, ?)",
    )
    .bind(project_id)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

async fn release_project_route_lock(pool: &SqlitePool, project_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM project_route_locks WHERE project_id = ?")
        .bind(project_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn route_item_semantic(deps: &RouteDeps<'_>, item: BacklogItem) -> Result<RouteOneResult> {
    let candidates = candidate_issues(deps.pool, deps.project.id).await?;
    if candidates.is_empty() {
        let issue_id = create_issue_from_item(deps.pool, &item, None, None, deps.now).await?;
        return Ok(RouteOneResult {
            outcome: RouteOutcome::CreatedIssue {
                item_id: item.id,
                issue_id,
            },
            fallback_reason: Some("no queue-capable candidate issues".to_string()),
        });
    }

    if let Some(issue) = deterministic_duplicate_candidate(&item, &candidates) {
        let note = format!(
            "Duplicate backlog item #{} was folded into issue #{} during routing.",
            item.id, issue.id
        );
        let message_id = attach_item_to_issue(deps.pool, &item, issue.id, &note, deps.now)
            .await
            .with_context(|| {
                format!(
                    "attaching duplicate backlog item {} to issue {}",
                    item.id, issue.id
                )
            })?;
        return Ok(RouteOneResult {
            outcome: RouteOutcome::AttachedToIssue {
                item_id: item.id,
                issue_id: issue.id,
                message_id,
            },
            fallback_reason: None,
        });
    }

    let route_run = run_route_agent(deps, &item, &candidates).await;
    let decision = match route_run {
        Ok(decision) => decision,
        Err(reason) => {
            if let Some(outcome) = attach_duplicate_if_present(deps, &item).await? {
                return Ok(outcome);
            }
            let issue_id =
                create_issue_from_item(deps.pool, &item, None, Some(&reason), deps.now).await?;
            return Ok(RouteOneResult {
                outcome: RouteOutcome::CreatedIssue {
                    item_id: item.id,
                    issue_id,
                },
                fallback_reason: Some(reason),
            });
        }
    };

    match decision {
        RouteDecision::Attach { issue_id, message } => {
            let allowed: HashSet<i64> = candidates.iter().map(|issue| issue.id).collect();
            if !allowed.contains(&issue_id) {
                let reason = format!("router chose non-candidate issue {issue_id}");
                if let Some(outcome) = attach_duplicate_if_present(deps, &item).await? {
                    return Ok(outcome);
                }
                let issue_id =
                    create_issue_from_item(deps.pool, &item, None, Some(&reason), deps.now).await?;
                return Ok(RouteOneResult {
                    outcome: RouteOutcome::CreatedIssue {
                        item_id: item.id,
                        issue_id,
                    },
                    fallback_reason: Some(reason),
                });
            }
            let message_id = attach_item_to_issue(deps.pool, &item, issue_id, &message, deps.now)
                .await
                .with_context(|| {
                    format!("attaching backlog item {} to issue {issue_id}", item.id)
                })?;
            Ok(RouteOneResult {
                outcome: RouteOutcome::AttachedToIssue {
                    item_id: item.id,
                    issue_id,
                    message_id,
                },
                fallback_reason: None,
            })
        }
        RouteDecision::Create { title, description } => {
            if let Some(outcome) = attach_duplicate_if_present(deps, &item).await? {
                return Ok(outcome);
            }
            let issue_id = create_issue_from_item(
                deps.pool,
                &item,
                title.as_deref(),
                description.as_deref(),
                deps.now,
            )
            .await?;
            Ok(RouteOneResult {
                outcome: RouteOutcome::CreatedIssue {
                    item_id: item.id,
                    issue_id,
                },
                fallback_reason: None,
            })
        }
    }
}

async fn attach_duplicate_if_present(
    deps: &RouteDeps<'_>,
    item: &BacklogItem,
) -> Result<Option<RouteOneResult>> {
    let candidates = candidate_issues(deps.pool, deps.project.id).await?;
    let Some(issue) = deterministic_duplicate_candidate(item, &candidates) else {
        return Ok(None);
    };
    let note = format!(
        "Duplicate backlog item #{} was folded into issue #{} during routing.",
        item.id, issue.id
    );
    let message_id = attach_item_to_issue(deps.pool, item, issue.id, &note, deps.now)
        .await
        .with_context(|| {
            format!(
                "attaching duplicate backlog item {} to issue {}",
                item.id, issue.id
            )
        })?;
    Ok(Some(RouteOneResult {
        outcome: RouteOutcome::AttachedToIssue {
            item_id: item.id,
            issue_id: issue.id,
            message_id,
        },
        fallback_reason: None,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum RouteDecision {
    Attach {
        issue_id: i64,
        message: String,
    },
    Create {
        title: Option<String>,
        description: Option<String>,
    },
}

async fn run_route_agent(
    deps: &RouteDeps<'_>,
    item: &BacklogItem,
    candidates: &[Issue],
) -> std::result::Result<RouteDecision, String> {
    let started_at = deps.now;
    let (log_path, prompt_path) =
        artifacts::routing_run_paths(deps.project.id, item.id, started_at)
            .map_err(|e| format!("preparing routing artifacts: {e:#}"))?;
    let prompt = route_prompt(item, candidates);
    std::fs::write(&prompt_path, &prompt)
        .map_err(|e| format!("writing route prompt {}: {e}", prompt_path.display()))?;
    let candidate_json =
        serde_json::to_string(&candidates.iter().map(|i| i.id).collect::<Vec<_>>())
            .map_err(|e| format!("encoding candidate ids: {e}"))?;
    let cmd = agent::expand_cmd_template(
        deps.project.route_agent_cmd(),
        agent::AgentTemplateVars {
            daemon_socket: Some(deps.socket),
            control_dir: None,
        },
    );
    let prompt_str = prompt_path.to_string_lossy().to_string();
    let log_str = log_path.to_string_lossy().to_string();
    let run_id = routing_runs::start(
        deps.pool,
        StartRoutingRun {
            project_id: deps.project.id,
            backlog_item_id: item.id,
            candidate_issue_ids: &candidate_json,
            agent_cmd: &cmd,
            prompt_path: Some(&prompt_str),
            log_path: Some(&log_str),
        },
        started_at,
    )
    .await
    .map_err(|e| format!("recording route run start: {e:#}"))?;

    let outcome = match deps
        .executor
        .execute(AgentSpec {
            cmd_template: &cmd,
            prompt: &prompt,
            cwd: Path::new(&deps.project.repo_path),
            log_path: &log_path,
            timeout: Duration::from_secs(ROUTE_TIMEOUT_SECS),
            env: &[],
        })
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            let reason = format!("route agent execution failed: {e:#}");
            let _ = routing_runs::finish(
                deps.pool,
                run_id,
                FinishRoutingRun {
                    raw_decision: None,
                    parsed_decision: None,
                    fallback_reason: Some(&reason),
                    exit_code: None,
                    exit_kind: Some(ExitKind::Error),
                    exited_at: deps.now,
                },
            )
            .await;
            return Err(reason);
        }
    };
    let raw = std::fs::read_to_string(&log_path)
        .ok()
        .and_then(|log| extract_last_agent_message(&log).or(Some(log)));
    let parsed = raw
        .as_deref()
        .ok_or_else(|| "route agent produced no output".to_string())
        .and_then(parse_route_decision);
    let (parsed_json, fallback_reason) = match &parsed {
        Ok(decision) => (
            serde_json::to_string(decision).ok(),
            route_outcome_fallback(&outcome),
        ),
        Err(reason) => (None, Some(reason.clone())),
    };
    let finish_reason = fallback_reason
        .clone()
        .or_else(|| route_outcome_fallback(&outcome));
    routing_runs::finish(
        deps.pool,
        run_id,
        FinishRoutingRun {
            raw_decision: raw.as_deref(),
            parsed_decision: parsed_json.as_deref(),
            fallback_reason: finish_reason.as_deref(),
            exit_code: outcome.exit_code.map(i64::from),
            exit_kind: Some(outcome.exit_kind),
            exited_at: deps.now,
        },
    )
    .await
    .map_err(|e| format!("recording route run finish: {e:#}"))?;
    if let Some(reason) = route_outcome_fallback(&outcome) {
        return Err(reason);
    }
    parsed
}

fn route_outcome_fallback(outcome: &agent::AgentOutcome) -> Option<String> {
    if outcome.exit_kind != ExitKind::Exited || outcome.exit_code != Some(0) {
        return Some(format!(
            "route agent ended with {} {:?}",
            outcome.exit_kind.as_str(),
            outcome.exit_code
        ));
    }
    None
}

fn route_prompt(item: &BacklogItem, candidates: &[Issue]) -> String {
    let mut s = String::new();
    s.push_str("You are routing one approved backlog item for auwsx.\n");
    s.push_str("Choose whether it belongs to an existing issue or should become a new issue.\n");
    s.push_str("Do not modify files or run commands. Return only strict JSON.\n\n");
    s.push_str("Allowed JSON shapes:\n");
    s.push_str(
        "{\"action\":\"attach\",\"issue_id\":123,\"message\":\"queue message for worker\"}\n",
    );
    s.push_str("{\"action\":\"create\",\"title\":\"short issue title\",\"description\":\"optional detail\"}\n\n");
    s.push_str("Rules:\n");
    s.push_str("- Attach only when the backlog is clearly relevant to one candidate issue.\n");
    s.push_str("- Attach duplicate or follow-up work to NEW/PLAN_READY candidates instead of creating another issue; auwsx will replan them if needed.\n");
    s.push_str("- If uncertain, create a new issue.\n");
    s.push_str("- The attach message must state the requested work and why it belongs with that issue.\n\n");
    s.push_str("Backlog item:\n");
    s.push_str(&format!("#{} {}\n\n", item.id, item.text.trim()));
    s.push_str("Candidate issues:\n");
    for issue in candidates {
        s.push_str(&format!(
            "- #{} [{}] {}\n",
            issue.id,
            issue.status.as_str(),
            issue.title
        ));
        if let Some(description) = issue
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            s.push_str(&format!("  description: {}\n", one_line(description)));
        }
        if let Some(summary) = issue
            .agent_summary
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            s.push_str(&format!("  summary: {}\n", one_line(summary)));
        }
        if let Some(progress) = issue
            .progress_report
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            s.push_str(&format!("  progress: {}\n", one_line(progress)));
        }
    }
    s
}

fn one_line(value: &str) -> String {
    const MAX: usize = 240;
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX {
        return compact;
    }
    let mut out = compact.chars().take(MAX - 3).collect::<String>();
    out.push_str("...");
    out
}

fn parse_route_decision(raw: &str) -> std::result::Result<RouteDecision, String> {
    let text = strip_code_fence(raw.trim());
    serde_json::from_str::<RouteDecision>(text)
        .map_err(|e| format!("route agent returned invalid decision JSON: {e}"))
        .and_then(|decision| match decision {
            RouteDecision::Attach { issue_id, message }
                if issue_id > 0 && !message.trim().is_empty() =>
            {
                Ok(RouteDecision::Attach {
                    issue_id,
                    message: message.trim().to_string(),
                })
            }
            RouteDecision::Create { title, description } => Ok(RouteDecision::Create {
                title: title.and_then(|v| normalized_optional(v.as_str())),
                description: description.and_then(|v| normalized_optional(v.as_str())),
            }),
            RouteDecision::Attach { .. } => {
                Err("attach decisions require positive issue_id and nonempty message".to_string())
            }
        })
}

fn strip_code_fence(raw: &str) -> &str {
    let Some(stripped) = raw.strip_prefix("```") else {
        return raw;
    };
    let without_lang = stripped
        .strip_prefix("json")
        .or_else(|| stripped.strip_prefix("JSON"))
        .unwrap_or(stripped)
        .trim_start();
    without_lang
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or(raw)
}

fn extract_last_agent_message(text: &str) -> Option<String> {
    let mut last_agent_message = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(item) = value.get("item") else {
            continue;
        };
        if item.get("type").and_then(|v| v.as_str()) == Some("agent_message") {
            if let Some(msg) = item.get("text").and_then(|v| v.as_str()) {
                last_agent_message = Some(msg.to_string());
            }
        }
    }
    last_agent_message
}

async fn candidate_issues(pool: &SqlitePool, project_id: i64) -> Result<Vec<Issue>> {
    Ok(issues::list_by_project(pool, project_id)
        .await?
        .into_iter()
        .filter(|issue| issue.status.accepts_queue_message())
        .collect())
}

fn deterministic_duplicate_candidate<'a>(
    item: &BacklogItem,
    candidates: &'a [Issue],
) -> Option<&'a Issue> {
    let item_tokens = normalized_tokens(&item.text);
    if item_tokens.len() < DUPLICATE_TOKEN_OVERLAP_MIN {
        return None;
    }

    candidates
        .iter()
        .filter_map(|issue| {
            let mut issue_text = issue.title.clone();
            if let Some(description) = issue.description.as_deref() {
                issue_text.push(' ');
                issue_text.push_str(description);
            }
            let issue_tokens = normalized_tokens(&issue_text);
            duplicate_score(&item_tokens, &issue_tokens).map(|score| (score, issue))
        })
        .filter(|(score, _)| score.overlap >= DUPLICATE_TOKEN_OVERLAP_MIN)
        .filter(|(score, _)| score.jaccard >= DUPLICATE_TOKEN_SCORE_MIN)
        .max_by(|(a, a_issue), (b, b_issue)| {
            a.jaccard
                .total_cmp(&b.jaccard)
                .then_with(|| a.overlap.cmp(&b.overlap))
                .then_with(|| b_issue.id.cmp(&a_issue.id))
        })
        .map(|(_, issue)| issue)
}

#[derive(Debug, Clone, Copy)]
struct DuplicateScore {
    overlap: usize,
    jaccard: f32,
}

fn duplicate_score(item_tokens: &[String], issue_tokens: &[String]) -> Option<DuplicateScore> {
    if item_tokens.is_empty() || issue_tokens.is_empty() {
        return None;
    }
    let item: HashSet<&str> = item_tokens.iter().map(String::as_str).collect();
    let issue: HashSet<&str> = issue_tokens.iter().map(String::as_str).collect();
    let overlap = item.intersection(&issue).count();
    let union = item.union(&issue).count();
    (union > 0).then(|| DuplicateScore {
        overlap,
        jaccard: overlap as f32 / union as f32,
    })
}

fn normalized_tokens(text: &str) -> Vec<String> {
    let stop_words: HashSet<&str> = [
        "the",
        "and",
        "for",
        "with",
        "that",
        "this",
        "from",
        "into",
        "then",
        "when",
        "what",
        "where",
        "which",
        "should",
        "would",
        "could",
        "need",
        "needs",
        "make",
        "made",
        "add",
        "adds",
        "implement",
        "implementation",
        "issue",
        "backlog",
    ]
    .into_iter()
    .collect();
    let mut tokens: Vec<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|token| token.len() >= 3)
        .map(str::to_ascii_lowercase)
        .filter(|token| !stop_words.contains(token.as_str()))
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

async fn create_issue_from_item(
    pool: &SqlitePool,
    item: &BacklogItem,
    title: Option<&str>,
    description: Option<&str>,
    now: i64,
) -> Result<i64> {
    let title = title
        .and_then(normalized_optional)
        .unwrap_or_else(|| issue_title_from_backlog(&item.text));
    let description = description
        .and_then(normalized_optional)
        .or_else(|| (item.text.trim() != title).then(|| item.text.trim().to_string()));
    let mut tx = pool.begin().await?;
    let issue_id: i64 = sqlx::query(
        "INSERT INTO issues (project_id, title, description, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(item.project_id)
    .bind(&title)
    .bind(description.as_deref())
    .bind(issues::INITIAL_STATUS.as_str())
    .bind(now)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?
    .get("id");
    let n = sqlx::query(
        "UPDATE backlog_items
         SET consumed_issue_id = ?, resolved_at = ?
         WHERE id = ? AND consumed_issue_id IS NULL",
    )
    .bind(issue_id)
    .bind(now)
    .bind(item.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        bail!("backlog item {} was already consumed", item.id);
    }
    tx.commit().await?;
    Ok(issue_id)
}

async fn attach_item_to_issue(
    pool: &SqlitePool,
    item: &BacklogItem,
    issue_id: i64,
    note: &str,
    now: i64,
) -> Result<i64> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT status FROM issues WHERE id = ? AND project_id = ?")
        .bind(issue_id)
        .bind(item.project_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        bail!("candidate issue {issue_id} not found");
    };
    let raw_status: String = row.try_get("status")?;
    let status = IssueStatus::from_str(&raw_status)
        .ok_or_else(|| anyhow!("unknown issue status {raw_status:?}"))?;
    if !status.accepts_queue_message() {
        bail!("issue {issue_id} no longer accepts queue messages");
    }

    let message_id: i64 = sqlx::query(
        "INSERT INTO steering (issue_id, source, note, created_at)
         VALUES (?, ?, ?, ?)
         RETURNING id",
    )
    .bind(issue_id)
    .bind(SteeringSource::Consolidation.as_str())
    .bind(note.trim())
    .bind(now)
    .fetch_one(&mut *tx)
    .await?
    .get("id");

    let next_status = if status == IssueStatus::ReadyToMerge {
        IssueStatus::Working
    } else if matches!(status, IssueStatus::PlanReady | IssueStatus::PlanBlocked) {
        IssueStatus::Planning
    } else {
        status
    };
    sqlx::query(
        "UPDATE issues
         SET status = ?, has_pending_steering = 1, wait_until = NULL, updated_at = ?
         WHERE id = ?",
    )
    .bind(next_status.as_str())
    .bind(now)
    .bind(issue_id)
    .execute(&mut *tx)
    .await?;

    let n = sqlx::query(
        "UPDATE backlog_items
         SET consumed_issue_id = ?, resolved_at = ?
         WHERE id = ? AND consumed_issue_id IS NULL",
    )
    .bind(issue_id)
    .bind(now)
    .bind(item.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        bail!("backlog item {} was already consumed", item.id);
    }
    tx.commit().await?;
    Ok(message_id)
}

fn normalized_optional(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn issue_title_from_backlog(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Untitled backlog");
    const MAX_CHARS: usize = 96;
    if first.chars().count() <= MAX_CHARS {
        return first.to_string();
    }
    let mut out = first.chars().take(MAX_CHARS - 3).collect::<String>();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentOutcome, ExitKind};
    use crate::backlog::Source;
    use crate::db::projects::{self, NewProject};
    use crate::db::Db;
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    struct RouteFake {
        response: &'static str,
    }

    struct FailingRouteFake;

    struct PanicRouteFake;

    struct LateDuplicateFake {
        db: Db,
        project_id: i64,
    }

    #[async_trait]
    impl AgentExecutor for RouteFake {
        async fn execute(&self, spec: AgentSpec<'_>) -> Result<AgentOutcome> {
            std::fs::write(
                spec.log_path,
                format!(
                    "{{\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
                    serde_json::to_string(self.response)?
                ),
            )?;
            Ok(AgentOutcome {
                exit_kind: ExitKind::Exited,
                exit_code: Some(0),
                pid: None,
            })
        }
    }

    #[async_trait]
    impl AgentExecutor for FailingRouteFake {
        async fn execute(&self, _spec: AgentSpec<'_>) -> Result<AgentOutcome> {
            Err(anyhow!("route agent unavailable"))
        }
    }

    #[async_trait]
    impl AgentExecutor for PanicRouteFake {
        async fn execute(&self, _spec: AgentSpec<'_>) -> Result<AgentOutcome> {
            panic!("deterministic duplicate routing should not invoke route agent")
        }
    }

    #[async_trait]
    impl AgentExecutor for LateDuplicateFake {
        async fn execute(&self, spec: AgentSpec<'_>) -> Result<AgentOutcome> {
            issues::create(
                self.db.pool(),
                self.project_id,
                "Implement route lock late duplicate smoke",
                Some(
                    "Fold similar approved scheduler routing backlog into one issue while preserving consumed rows under concurrency.",
                ),
                3,
            )
            .await?;
            std::fs::write(
                spec.log_path,
                format!(
                    "{{\"item\":{{\"type\":\"agent_message\",\"text\":{}}}}}\n",
                    serde_json::to_string(
                        r#"{"action":"create","title":"Implement route lock late duplicate smoke","description":"Fold similar approved scheduler routing backlog into one issue while preserving consumed rows under concurrency."}"#
                    )?
                ),
            )?;
            Ok(AgentOutcome {
                exit_kind: ExitKind::Exited,
                exit_code: Some(0),
                pid: None,
            })
        }
    }

    async fn project(db: &Db) -> Result<Project> {
        let id = projects::create(
            db.pool(),
            NewProject {
                name: "demo",
                repo_path: ".",
                default_branch: "main",
                arsenal_preset_name: None,
                main_agent_cmd: "noop {prompt}",
                route_agent_cmd: "noop {prompt}",
                plan_agent_cmd: "noop {prompt}",
                work_agent_cmd: "noop {prompt}",
                review_agent_cmd: None,
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
                schedule_interval_min: None,
                schedule_cron: None,
            },
            1,
        )
        .await?;
        Ok(projects::get(db.pool(), id).await?.expect("project exists"))
    }

    fn deps<'a, E: AgentExecutor>(
        db: &'a Db,
        project: &'a Project,
        fake: &'a E,
        now: i64,
    ) -> RouteDeps<'a> {
        RouteDeps {
            pool: db.pool(),
            project,
            executor: fake,
            socket: Path::new("/tmp/auwsx-test.sock"),
            now,
        }
    }

    #[test]
    fn given_agent_json_when_parse_route_decision_then_attach_is_normalized() {
        let got = parse_route_decision(
            r#"{"action":"attach","issue_id":7,"message":"  add cursor support  "}"#,
        )
        .unwrap();
        assert_eq!(
            got,
            RouteDecision::Attach {
                issue_id: 7,
                message: "add cursor support".to_string()
            }
        );
    }

    #[test]
    fn given_fenced_agent_json_when_parse_route_decision_then_create_is_accepted() {
        let got = parse_route_decision(
            "```json\n{\"action\":\"create\",\"title\":\"new settings\",\"description\":\"\"}\n```",
        )
        .unwrap();
        assert_eq!(
            got,
            RouteDecision::Create {
                title: Some("new settings".to_string()),
                description: None
            }
        );
    }

    #[test]
    fn given_empty_attach_message_when_parse_route_decision_then_reject() {
        let err = parse_route_decision(r#"{"action":"attach","issue_id":7,"message":" "}"#)
            .expect_err("empty attach message must be rejected");
        assert!(err.contains("attach decisions require"));
    }

    #[tokio::test]
    async fn given_matching_backlog_when_semantic_route_then_attaches_to_working_issue(
    ) -> Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let project = project(&db).await?;
        let issue_id = issues::create(db.pool(), project.id, "settings ui", None, 1).await?;
        issues::force_status(db.pool(), issue_id, IssueStatus::Working, 2).await?;
        let backlog_id = backlog::add(
            db.pool(),
            project.id,
            "Improve settings UI cursor positioning",
            Source::Human,
            None,
            3,
        )
        .await?;
        let fake = RouteFake {
            response: r#"{"action":"attach","issue_id":1,"message":"Add cursor positioning to the settings UI work."}"#,
        };

        let routed = route_approved_project_semantic(&deps(&db, &project, &fake, 4)).await?;

        assert_eq!(routed.len(), 1);
        assert!(matches!(
            routed[0].outcome,
            RouteOutcome::AttachedToIssue { issue_id: 1, .. }
        ));
        let item = backlog::get(db.pool(), backlog_id)
            .await?
            .expect("item exists");
        assert_eq!(item.consumed_issue_id, Some(issue_id));
        let pending = crate::steering::list_pending(db.pool(), issue_id).await?;
        assert_eq!(pending.len(), 1);
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_ready_to_merge_issue_when_backlog_attaches_then_issue_returns_to_working(
    ) -> Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let project = project(&db).await?;
        let issue_id = issues::create(db.pool(), project.id, "archive view", None, 1).await?;
        issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, 2).await?;
        backlog::add(
            db.pool(),
            project.id,
            "Add log scrolling to the archive view implementation",
            Source::Human,
            None,
            3,
        )
        .await?;
        let fake = RouteFake {
            response: r#"{"action":"attach","issue_id":1,"message":"Fold log scrolling into archive view before merge."}"#,
        };

        route_approved_project_semantic(&deps(&db, &project, &fake, 4)).await?;

        let issue = issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists");
        assert_eq!(issue.status, IssueStatus::Working);
        assert!(issue.has_pending_steering);
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_duplicate_plan_ready_issue_when_semantic_route_then_replans_without_agent(
    ) -> Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let project = project(&db).await?;
        let issue_id = issues::create(
            db.pool(),
            project.id,
            "Implement dirty-main merge safety helper",
            Some(
                "Add an issue apply-merge helper that snapshots dirty tracked and untracked main workspace state, merges the issue branch, restores dirty state, marks issues DONE or CONFLICT_BLOCKED, and verifies behavior with git fixture tests.",
            ),
            1,
        )
        .await?;
        issues::force_status(db.pool(), issue_id, IssueStatus::PlanReady, 2).await?;
        let backlog_id = backlog::add(
            db.pool(),
            project.id,
            "Implement deterministic dirty-main local merge safety: add issue apply-merge helper that snapshots dirty tracked/untracked main state, merges issue branch, restores dirty state, marks DONE or CONFLICT_BLOCKED, and verifies with git fixture tests.",
            Source::Human,
            None,
            3,
        )
        .await?;

        let routed =
            route_approved_project_semantic(&deps(&db, &project, &PanicRouteFake, 4)).await?;

        assert_eq!(routed.len(), 1);
        assert!(matches!(
            routed[0].outcome,
            RouteOutcome::AttachedToIssue {
                issue_id: got, ..
            } if got == issue_id
        ));
        let item = backlog::get(db.pool(), backlog_id)
            .await?
            .expect("item exists");
        assert_eq!(item.consumed_issue_id, Some(issue_id));
        let issue = issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists");
        assert_eq!(issue.status, IssueStatus::Planning);
        assert!(issue.has_pending_steering);
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_similar_backlogs_in_one_batch_when_semantic_route_then_second_attaches_to_first(
    ) -> Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let project = project(&db).await?;
        let first = backlog::add(
            db.pool(),
            project.id,
            "Implement deterministic dirty-main local merge safety helper with snapshot restore and conflict blocked status.",
            Source::Human,
            None,
            3,
        )
        .await?;
        let second = backlog::add(
            db.pool(),
            project.id,
            "Implement dirty-main merge safety helper with dirty snapshot restore and CONFLICT_BLOCKED handling.",
            Source::Human,
            None,
            4,
        )
        .await?;
        let fake = RouteFake {
            response: r#"{"action":"create","title":"Implement dirty-main merge safety helper","description":"Implement deterministic dirty-main local merge safety helper with snapshot restore and conflict blocked status."}"#,
        };

        let routed = route_approved_project_semantic(&deps(&db, &project, &fake, 5)).await?;

        assert_eq!(routed.len(), 2);
        let first_issue_id = routed[0].outcome.issue_id();
        assert!(matches!(
            routed[0].outcome,
            RouteOutcome::CreatedIssue { .. }
        ));
        assert!(matches!(
            routed[1].outcome,
            RouteOutcome::AttachedToIssue {
                issue_id, ..
            } if issue_id == first_issue_id
        ));
        assert_eq!(
            backlog::get(db.pool(), first)
                .await?
                .expect("first item")
                .consumed_issue_id,
            Some(first_issue_id)
        );
        assert_eq!(
            backlog::get(db.pool(), second)
                .await?
                .expect("second item")
                .consumed_issue_id,
            Some(first_issue_id)
        );
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_project_route_lock_held_when_semantic_route_then_skips_without_agent(
    ) -> Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let project = project(&db).await?;
        backlog::add(
            db.pool(),
            project.id,
            "Implement serialized routing",
            Source::Human,
            None,
            3,
        )
        .await?;
        assert!(acquire_project_route_lock(db.pool(), project.id, 4).await?);

        let routed =
            route_approved_project_semantic(&deps(&db, &project, &PanicRouteFake, 5)).await?;

        assert!(routed.is_empty());
        release_project_route_lock(db.pool(), project.id).await?;
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_stale_project_route_lock_when_semantic_route_then_lock_is_recovered(
    ) -> Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let project = project(&db).await?;
        backlog::add(
            db.pool(),
            project.id,
            "Implement stale lock recovery",
            Source::Human,
            None,
            3,
        )
        .await?;
        assert!(acquire_project_route_lock(db.pool(), project.id, 4).await?);
        let fake = RouteFake {
            response: r#"{"action":"create","title":"stale lock recovery","description":""}"#,
        };

        let routed = route_approved_project_semantic(&deps(
            &db,
            &project,
            &fake,
            4 + ROUTE_LOCK_STALE_MS + 1,
        ))
        .await?;

        assert_eq!(routed.len(), 1);
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_matching_issue_appears_during_route_when_agent_says_create_then_attaches(
    ) -> Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let project = project(&db).await?;
        let unrelated =
            issues::create(db.pool(), project.id, "unrelated candidate", None, 1).await?;
        issues::force_status(db.pool(), unrelated, IssueStatus::Working, 2).await?;
        let backlog_id = backlog::add(
            db.pool(),
            project.id,
            "Implement route lock late duplicate smoke by folding similar approved scheduler routing backlog into one issue while preserving consumed rows under concurrency.",
            Source::Human,
            None,
            3,
        )
        .await?;
        let fake = LateDuplicateFake {
            db: db.clone(),
            project_id: project.id,
        };

        let routed = route_approved_project_semantic(&deps(&db, &project, &fake, 4)).await?;

        assert_eq!(routed.len(), 1);
        let target_id = routed[0].outcome.issue_id();
        assert_ne!(target_id, unrelated);
        assert!(matches!(
            routed[0].outcome,
            RouteOutcome::AttachedToIssue { .. }
        ));
        assert_eq!(
            backlog::get(db.pool(), backlog_id)
                .await?
                .expect("item exists")
                .consumed_issue_id,
            Some(target_id)
        );
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_invalid_router_output_when_semantic_route_then_creates_issue_with_fallback(
    ) -> Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let project = project(&db).await?;
        let issue_id = issues::create(db.pool(), project.id, "existing", None, 1).await?;
        issues::force_status(db.pool(), issue_id, IssueStatus::Working, 2).await?;
        let backlog_id = backlog::add(
            db.pool(),
            project.id,
            "Unclear work",
            Source::Human,
            None,
            3,
        )
        .await?;
        let fake = RouteFake {
            response: "not json",
        };

        let routed = route_approved_project_semantic(&deps(&db, &project, &fake, 4)).await?;

        assert!(matches!(
            routed[0].outcome,
            RouteOutcome::CreatedIssue { .. }
        ));
        assert!(routed[0]
            .fallback_reason
            .as_deref()
            .unwrap_or_default()
            .contains("invalid decision JSON"));
        let item = backlog::get(db.pool(), backlog_id)
            .await?
            .expect("item exists");
        assert_ne!(item.consumed_issue_id, Some(issue_id));
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_router_executor_error_when_semantic_route_then_fallback_is_recorded(
    ) -> Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());
        let db = Db::open_memory().await?;
        let project = project(&db).await?;
        let issue_id = issues::create(db.pool(), project.id, "existing", None, 1).await?;
        issues::force_status(db.pool(), issue_id, IssueStatus::Working, 2).await?;
        backlog::add(
            db.pool(),
            project.id,
            "Work that needs routing",
            Source::Human,
            None,
            3,
        )
        .await?;
        let fake = FailingRouteFake;

        let routed = route_approved_project_semantic(&deps(&db, &project, &fake, 4)).await?;

        assert!(matches!(
            routed[0].outcome,
            RouteOutcome::CreatedIssue { .. }
        ));
        assert!(routed[0]
            .fallback_reason
            .as_deref()
            .unwrap_or_default()
            .contains("route agent execution failed"));
        let runs = routing_runs::recent_by_project(db.pool(), project.id, 1).await?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].exit_kind, Some(ExitKind::Error));
        assert!(runs[0]
            .fallback_reason
            .as_deref()
            .unwrap_or_default()
            .contains("route agent unavailable"));
        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }
}
