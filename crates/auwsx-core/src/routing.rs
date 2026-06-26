//! Backlog routing service.
//!
//! This is the single entry point for turning approved backlog into executable
//! work. Today it creates standalone issues. The planned extension is to attach
//! relevant backlog to an existing issue as a queue message when the issue is in
//! an attachable status.

use crate::backlog::{self, Approval, BacklogItem};
use crate::db::issues;
use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

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
}

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
        let issue_id = create_issue_from_item(pool, &item, now).await?;
        routed.push(RouteOutcome::CreatedIssue {
            item_id: item.id,
            issue_id,
        });
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
    create_issue_from_item(pool, &item, now).await
}

async fn create_issue_from_item(pool: &SqlitePool, item: &BacklogItem, now: i64) -> Result<i64> {
    let title = issue_title_from_backlog(&item.text);
    let description = (item.text.trim() != title).then_some(item.text.trim());
    let issue_id = issues::create(pool, item.project_id, &title, description, now).await?;
    backlog::mark_consumed(pool, item.id, issue_id, now).await?;
    Ok(issue_id)
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
