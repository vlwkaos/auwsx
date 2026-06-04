//! Drafts inbox + triage execution. Plan Step 3.7.
//!
//! Drafts are lightweight intent dumps: just `(project_id, body, created_at)`.
//! No agent, no worktree, no status enum. The triage main-job consumes pending
//! drafts and emits per-draft actions: KEEP_AS_TASK / MERGE_INTO / SPLIT_INTO / DISCARD.
//!
//! Triage prompt + flow:
//!   1. App writes `<repo>/.auwsx/triage/drafts.json` + `open-tasks.md`.
//!   2. App fires triage prompt via main-agent.
//!   3. App reads `decisions.json` and applies transactionally:
//!      - INSERT tasks (KEEP_AS_TASK / SPLIT_INTO)
//!      - INSERT followups (MERGE_INTO)
//!      - mark drafts consumed/discarded
//!   4. On malformed JSON or agent failure → drafts untouched, log error.

use crate::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftState {
    Pending,
    Consumed,
    Discarded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub id: i64,
    pub project_id: i64,
    pub body: String,
    pub state: DraftState,
    pub consumed_by: Option<String>,
    pub discard_reason: Option<String>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TriageDecision {
    KeepAsTask {
        draft_id: i64,
        title: String,
        description: Option<String>,
    },
    MergeInto {
        draft_id: i64,
        target_task_id: i64,
        note: String,
    },
    SplitInto {
        draft_id: i64,
        tasks: Vec<NewTaskSpec>,
    },
    Discard {
        draft_id: i64,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTaskSpec {
    pub title: String,
    pub description: Option<String>,
}

// TODO: create/list/get/delete CRUD
// TODO: run_triage(project_id) — write artifacts, fire prompt, parse decisions, apply tx
// TODO: TRIAGE_PROMPT constant from plan Step 3.7
