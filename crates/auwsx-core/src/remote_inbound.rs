//! Inbound remote repository event processing.
//!
//! Web adapters are responsible for transport details such as HTTP headers and
//! signature verification. This module owns deterministic daemon-side mutation:
//! delivery idempotency, per-project toggle checks, backlog insertion, remote
//! link recording, and audit rows.

use crate::backlog::{self, Source};
use crate::db::remote::{
    self, NewRemoteSyncRun, ProjectRemoteConfig, RecordRemoteEvent, RemoteEventStatus,
    RemoteProvider, RemoteSyncDirection, RemoteSyncKind, RemoteSyncStatus, UpsertRemoteIssueLink,
};
use crate::remote_plan::{self, InboundAuwsxRunDecision, InboundAuwsxRunIgnoreReason};
use crate::Result;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct ProcessRemoteAuwsxRunInput<'a> {
    pub provider: RemoteProvider,
    pub delivery_id: &'a str,
    pub event_kind: &'a str,
    pub action: Option<&'a str>,
    pub payload_hash: &'a str,
    pub owner: &'a str,
    pub repo: &'a str,
    pub remote_issue_number: i64,
    pub remote_issue_node_id: Option<&'a str>,
    pub remote_issue_title: &'a str,
    pub remote_issue_url: &'a str,
    pub comment_body: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum RemoteInboundOutcome {
    Accepted {
        event_id: i64,
        backlog_item_id: i64,
        remote_issue_link_id: i64,
        remote_sync_run_id: i64,
    },
    Ignored {
        event_id: Option<i64>,
        reason: InboundAuwsxRunIgnoreReason,
    },
    Duplicate {
        existing_event_id: Option<i64>,
    },
}

pub async fn process_remote_auwsx_run(
    pool: &SqlitePool,
    input: ProcessRemoteAuwsxRunInput<'_>,
    now: i64,
) -> Result<RemoteInboundOutcome> {
    let config = remote::get_config_by_repo(pool, input.provider, input.owner, input.repo).await?;
    let event_id = remote::record_event(
        pool,
        RecordRemoteEvent {
            project_id: config.as_ref().map(|config| config.project_id),
            provider: input.provider,
            delivery_id: input.delivery_id,
            event_kind: input.event_kind,
            action: input.action,
            payload_hash: input.payload_hash,
        },
        now,
    )
    .await?;
    let Some(event_id) = event_id else {
        let existing = remote::event_by_delivery(pool, input.provider, input.delivery_id).await?;
        return Ok(RemoteInboundOutcome::Duplicate {
            existing_event_id: existing.map(|event| event.id),
        });
    };

    let result = process_recorded_remote_auwsx_run(pool, event_id, config, &input, now).await;
    if let Err(e) = &result {
        let error = format!("{e:#}");
        remote::update_event_status(pool, event_id, RemoteEventStatus::Failed, Some(&error), now)
            .await?;
    }
    result
}

async fn process_recorded_remote_auwsx_run(
    pool: &SqlitePool,
    event_id: i64,
    config: Option<ProjectRemoteConfig>,
    input: &ProcessRemoteAuwsxRunInput<'_>,
    now: i64,
) -> Result<RemoteInboundOutcome> {
    match remote_plan::plan_inbound_auwsx_run(
        config.as_ref(),
        input.remote_issue_title,
        input.remote_issue_url,
        input.comment_body,
    ) {
        InboundAuwsxRunDecision::Ignore { reason } => {
            remote::update_event_status(
                pool,
                event_id,
                RemoteEventStatus::Ignored,
                Some(&format!("{reason:?}")),
                now,
            )
            .await?;
            Ok(RemoteInboundOutcome::Ignored {
                event_id: Some(event_id),
                reason,
            })
        }
        InboundAuwsxRunDecision::Accept { title, description } => {
            let config = config.context("accepted inbound run without project remote config")?;
            let text = format!("{title}\n\n{description}");
            let backlog_item_id =
                backlog::add(pool, config.project_id, &text, Source::Inbox, None, now).await?;
            let remote_issue_link_id = remote::upsert_issue_link(
                pool,
                UpsertRemoteIssueLink {
                    project_id: config.project_id,
                    issue_id: None,
                    backlog_item_id: Some(backlog_item_id),
                    provider: input.provider,
                    remote_owner: input.owner,
                    remote_repo: input.repo,
                    remote_issue_number: input.remote_issue_number,
                    remote_node_id: input.remote_issue_node_id,
                    remote_url: input.remote_issue_url,
                    last_synced_at: Some(now),
                },
                now,
            )
            .await?;
            let remote_sync_run_id = remote::create_sync_run(
                pool,
                NewRemoteSyncRun {
                    project_id: config.project_id,
                    issue_id: None,
                    backlog_item_id: Some(backlog_item_id),
                    remote_issue_link_id: Some(remote_issue_link_id),
                    remote_pr_link_id: None,
                    direction: RemoteSyncDirection::Inbound,
                    kind: RemoteSyncKind::Webhook,
                    status: RemoteSyncStatus::Done,
                    summary: Some("accepted remote /auwsx-run into approved backlog"),
                    error: None,
                    started_at: Some(now),
                    ended_at: Some(now),
                },
                now,
            )
            .await?;
            remote::update_event_status(pool, event_id, RemoteEventStatus::Processed, None, now)
                .await?;
            Ok(RemoteInboundOutcome::Accepted {
                event_id,
                backlog_item_id,
                remote_issue_link_id,
                remote_sync_run_id,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backlog::Approval;
    use crate::db::projects::{self, NewProject};
    use crate::db::remote::{RemoteAuthKind, RequiredChecksPolicy, UpsertProjectRemoteConfig};
    use crate::db::Db;

    const TS: i64 = 1_000;

    async fn project(db: &Db) -> i64 {
        projects::create(
            db.pool(),
            NewProject {
                name: "p",
                repo_path: ".",
                default_branch: "main",
                arsenal_preset_name: None,
                main_agent_cmd: "cat",
                route_agent_cmd: "cat",
                plan_agent_cmd: "cat",
                work_agent_cmd: "cat",
                review_agent_cmd: None,
                completion_policy: Some(projects::CompletionPolicy::Manual),
                plan_gate_timeout_min: Some(10),
                completion_soft_timeout_min: Some(60),
                schedule_interval_min: None,
                schedule_cron: None,
            },
            TS,
        )
        .await
        .unwrap()
    }

    async fn config(db: &Db, project_id: i64, enabled: bool) {
        remote::upsert_config(
            db.pool(),
            UpsertProjectRemoteConfig {
                project_id,
                provider: RemoteProvider::Github,
                remote_url: "https://github.com/acme/app",
                owner: "acme",
                repo: "app",
                api_base_url: "https://api.github.com",
                auth_kind: RemoteAuthKind::None,
                auth_ref: None,
                webhook_secret_ref: None,
                inbound_auwsx_run_enabled: enabled,
                outbound_issue_create_enabled: false,
                remote_pr_merge_enabled: false,
                agent_comment_sync_enabled: false,
                subtask_comment_sync_enabled: false,
                finding_comment_sync_enabled: false,
                draft_pr_enabled: false,
                required_checks_policy: RequiredChecksPolicy::Observe,
                default_labels: None,
                default_assignees: None,
                pr_base_branch: Some("main"),
            },
            TS,
        )
        .await
        .unwrap();
    }

    fn input(delivery_id: &str) -> ProcessRemoteAuwsxRunInput<'_> {
        ProcessRemoteAuwsxRunInput {
            provider: RemoteProvider::Github,
            delivery_id,
            event_kind: "issue_comment",
            action: Some("created"),
            payload_hash: "hash",
            owner: "acme",
            repo: "app",
            remote_issue_number: 9,
            remote_issue_node_id: Some("node-9"),
            remote_issue_title: "Remote task",
            remote_issue_url: "https://github.com/acme/app/issues/9",
            comment_body: "please\n/auwsx-run add inbound bridge",
        }
    }

    #[tokio::test]
    async fn given_enabled_inbound_run_when_processed_then_approved_backlog_and_link_are_created() {
        let db = Db::open_memory().await.unwrap();
        let project_id = project(&db).await;
        config(&db, project_id, true).await;

        let outcome = process_remote_auwsx_run(db.pool(), input("d1"), TS + 1)
            .await
            .unwrap();

        let RemoteInboundOutcome::Accepted {
            backlog_item_id,
            remote_issue_link_id,
            ..
        } = outcome
        else {
            panic!("expected accepted outcome");
        };
        let item = backlog::get(db.pool(), backlog_item_id)
            .await
            .unwrap()
            .expect("backlog exists");
        let link = remote::issue_link_by_backlog_item(db.pool(), backlog_item_id)
            .await
            .unwrap()
            .expect("remote issue link exists");
        let runs = remote::recent_sync_runs(db.pool(), project_id, 10)
            .await
            .unwrap();
        assert_eq!(
            (item.approval, item.source),
            (Approval::Approved, Source::Inbox)
        );
        assert!(item.text.contains("add inbound bridge"));
        assert_eq!(link.remote_issue_number, 9);
        assert!(runs.iter().any(|run| {
            run.kind == RemoteSyncKind::Webhook
                && run.status == RemoteSyncStatus::Done
                && run.remote_issue_link_id == Some(remote_issue_link_id)
        }));
    }

    #[tokio::test]
    async fn given_duplicate_delivery_when_processed_then_no_second_backlog_is_created() {
        let db = Db::open_memory().await.unwrap();
        let project_id = project(&db).await;
        config(&db, project_id, true).await;
        process_remote_auwsx_run(db.pool(), input("d1"), TS + 1)
            .await
            .unwrap();

        let outcome = process_remote_auwsx_run(db.pool(), input("d1"), TS + 2)
            .await
            .unwrap();

        let items = backlog::list_by_project(db.pool(), project_id)
            .await
            .unwrap();
        assert!(matches!(outcome, RemoteInboundOutcome::Duplicate { .. }));
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn given_disabled_inbound_run_when_processed_then_event_is_ignored() {
        let db = Db::open_memory().await.unwrap();
        let project_id = project(&db).await;
        config(&db, project_id, false).await;

        let outcome = process_remote_auwsx_run(db.pool(), input("d1"), TS + 1)
            .await
            .unwrap();

        let items = backlog::list_by_project(db.pool(), project_id)
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            RemoteInboundOutcome::Ignored {
                reason: InboundAuwsxRunIgnoreReason::Disabled,
                ..
            }
        ));
        assert!(items.is_empty());
    }
}
