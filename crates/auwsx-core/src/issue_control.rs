//! Pure issue/project operator policy. Runtime code in `scheduler` executes
//! these plans; this module decides what an operator intent means for status.

use crate::db::agent_runs::AgentRun;
use crate::db::issues::Issue;
use crate::pipeline;
use crate::state::IssueStatus;
use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueExecutePlan {
    RunPhase,
    RetryFailed { retry_status: IssueStatus },
    ApproveMerge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectExecutePlan {
    TickScheduler,
    ApproveReadyMergeQueue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOutcome {
    Ok,
    RanIssue { issue_id: i64 },
    ApprovedMerge { issue_ids: Vec<i64> },
}

pub fn plan_issue_execute(issue: &Issue, runs: &[AgentRun]) -> Result<IssueExecutePlan> {
    if issue.status == IssueStatus::ReadyToMerge {
        return Ok(IssueExecutePlan::ApproveMerge);
    }
    if issue.status == IssueStatus::Failed {
        return Ok(IssueExecutePlan::RetryFailed {
            retry_status: retry_status_from_runs(runs),
        });
    }
    if pipeline::plan_phase(issue.status).is_some() {
        return Ok(IssueExecutePlan::RunPhase);
    }
    bail!(
        "issue {} is not executable in status {}",
        issue.id,
        issue.status.as_str()
    )
}

pub fn plan_project_execute(ready_to_merge_count: usize) -> ProjectExecutePlan {
    if ready_to_merge_count > 0 {
        ProjectExecutePlan::ApproveReadyMergeQueue
    } else {
        ProjectExecutePlan::TickScheduler
    }
}

pub fn retry_status_from_runs(runs: &[AgentRun]) -> IssueStatus {
    runs.iter()
        .rev()
        .filter_map(|run| run.status_before.as_deref())
        .filter_map(IssueStatus::from_str)
        .find(|status| pipeline::plan_phase(*status).is_some())
        .unwrap_or(IssueStatus::New)
}
