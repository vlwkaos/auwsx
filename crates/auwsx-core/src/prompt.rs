//! Per-phase prompt construction.
//!
//! The pipeline is a state machine, not one mega-prompt: each actionable phase
//! gets a focused prompt that (1) gives the agent the issue context auwsx holds,
//! (2) states the phase's job, and (3) tells the agent exactly which control-CLI
//! command to run to advance the issue before it exits. auwsx never parses the
//! agent's prose — the status it sets via the CLI is the only signal that
//! matters (see `crate::state`).
//!
//! The agent reaches the daemon through the `auwsx` CLI; the pipeline injects
//! `AUWSX_SOCK` and `AUWSX_ISSUE_ID` into its environment, so every callback is
//! written as `auwsx … "$AUWSX_ISSUE_ID"`.
//!
//! These v1 templates are deliberately compact and agent-agnostic. Skill
//! mentions (`/backpressure`, `/good-to-go`) resolve natively for Claude; for
//! agents without slash-skills the pipeline will inline skill text (see
//! `skills`) — not yet wired.

use crate::db::findings::Finding;
use crate::db::issues::Issue;
use crate::db::subtasks::Subtask;
use crate::state::IssueStatus;
use crate::steering::Steering;
use std::fmt::Write as _;

/// Everything the prompt builder needs, loaded by the pipeline from the DB.
pub struct PromptContext<'a> {
    pub issue: &'a Issue,
    pub subtasks: &'a [Subtask],
    /// Pending (unconsumed) steering notes.
    pub steering: &'a [Steering],
    /// Open findings (the unresolved review set).
    pub open_findings: &'a [Finding],
}

/// Build the prompt for an issue's current (actionable) phase. Returns `None`
/// for non-actionable statuses (the scheduler never spawns those).
pub fn build(ctx: &PromptContext) -> Option<String> {
    let issue = ctx.issue;
    let body = match issue.status {
        IssueStatus::Consolidating => consolidating(ctx),
        IssueStatus::Planning => planning(ctx),
        IssueStatus::Implementing => implementing(ctx),
        IssueStatus::Review => review(ctx),
        IssueStatus::NeedsFix => needs_fix(ctx),
        IssueStatus::Audit => audit(ctx),
        IssueStatus::Conflicted => conflicted(ctx),
        IssueStatus::Completing => completing(ctx),
        _ => return None,
    };
    Some(format!("{}\n\n{}", header(ctx), body))
}

/// Shared context block: who/what, the issue, and the callback contract.
fn header(ctx: &PromptContext) -> String {
    let i = ctx.issue;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "You are an autonomous agent working ONE issue for auwsx. Do the phase's \
         job, then advance the issue with the control CLI and exit. auwsx reads \
         only the status you set — not this transcript."
    );
    let _ = writeln!(s, "\nISSUE #{}: {}", i.id, i.title);
    if let Some(d) = &i.description {
        let _ = writeln!(s, "Description: {d}");
    }
    let _ = writeln!(s, "Phase: {}", i.status.as_str());
    if !ctx.steering.is_empty() {
        let _ = writeln!(s, "\nPending steering (incorporate, do not ignore):");
        for st in ctx.steering {
            let _ = writeln!(s, "  - [{}] {}", st.source.as_str(), st.note);
        }
    }
    let _ = writeln!(
        s,
        "\nControl CLI (the env already points it at this daemon and issue):\n  \
         auwsx issue status \"$AUWSX_ISSUE_ID\" <STATUS>   # advance the issue"
    );
    s
}

fn subtask_block(ctx: &PromptContext) -> String {
    if ctx.subtasks.is_empty() {
        return String::new();
    }
    let mut s = String::from("\nPlan / subtasks:\n");
    for t in ctx.subtasks {
        let mark = if t.done { 'x' } else { ' ' };
        let _ = writeln!(s, "  [{}] {}. {}", mark, t.ord, t.text);
    }
    s
}

fn findings_block(ctx: &PromptContext) -> String {
    if ctx.open_findings.is_empty() {
        return String::new();
    }
    let mut s = String::from("\nOpen findings to adjudicate:\n");
    for f in ctx.open_findings {
        let _ = writeln!(
            s,
            "  #{} [{}] {}{}",
            f.id,
            f.severity.as_str(),
            f.title,
            f.file_ref
                .as_ref()
                .map(|r| format!("  ({r})"))
                .unwrap_or_default()
        );
    }
    s
}

fn consolidating(_ctx: &PromptContext) -> String {
    "JOB (consolidation, no worktree yet): decide whether this issue duplicates \
     or belongs inside an issue already in flight.\n\
     - List active issues: `auwsx issue ls <project_id>`.\n\
     - If a working-phase issue already covers this, fold this in as guidance \
       (`auwsx steering add <that_issue_id> consolidation \"<note>\"`) and then \
       self-close: `auwsx issue status \"$AUWSX_ISSUE_ID\" ABSORBED`.\n\
     - Otherwise proceed standalone: `auwsx issue status \"$AUWSX_ISSUE_ID\" PLANNING`."
        .to_string()
}

fn planning(ctx: &PromptContext) -> String {
    format!(
        "JOB (planning): produce a concrete implementation plan for this issue in \
         the current worktree.\n\
         - Investigate the codebase as needed; write the plan to `.auwsx/plan.md`.\n\
         - Record each step as a subtask: `auwsx subtask add \"$AUWSX_ISSUE_ID\" <ord> \"<text>\"`.\n\
         - Apply `/backpressure` thinking: resolve ambiguity before coding.\n\
         - When the plan is complete: `auwsx issue status \"$AUWSX_ISSUE_ID\" PLANNED`.\n\
         - If you are blocked and need a human: `auwsx issue status \"$AUWSX_ISSUE_ID\" PLAN_BLOCKED`.{}",
        subtask_block(ctx)
    )
}

fn implementing(ctx: &PromptContext) -> String {
    format!(
        "JOB (implementing): execute the plan in this worktree.\n\
         - Work through the subtasks; mark each done: `auwsx subtask done <subtask_id>`.\n\
         - Keep a running note in `.auwsx/progress.md`.\n\
         - Commit your work in the worktree as you go.\n\
         - When the implementation is complete and builds/tests pass: \
           `auwsx issue status \"$AUWSX_ISSUE_ID\" REVIEW`.{}",
        subtask_block(ctx)
    )
}

fn review(_ctx: &PromptContext) -> String {
    "JOB (review — you are a FRESH third eye / devil's advocate; do NOT defend \
     prior work): scrutinize the diff in this worktree for correctness, \
     simplicity, and security.\n\
     - File each problem: `auwsx finding add \"$AUWSX_ISSUE_ID\" <round> <severity> \"<title>\" \
       --detail \"<why>\" --file <path>`  (severity: blocker|major|minor|nit).\n\
     - If you filed any findings: `auwsx issue status \"$AUWSX_ISSUE_ID\" NEEDS_FIX`.\n\
     - If the work is clean: `auwsx issue status \"$AUWSX_ISSUE_ID\" AUDIT`."
        .to_string()
}

fn needs_fix(ctx: &PromptContext) -> String {
    format!(
        "JOB (adjudicate + fix): you are the implementer responding to review \
         findings. For EACH open finding, decide on the record:\n\
         - accept (you will fix it): `auwsx finding accept <finding_id> \"<how you'll fix>\"`\n\
         - reject (with reason): `auwsx finding reject <finding_id> \"<why it's not a problem>\"`\n\
         Then fix everything you accepted, commit, and hand back for re-review: \
         `auwsx issue status \"$AUWSX_ISSUE_ID\" REVIEW`.{}",
        findings_block(ctx)
    )
}

fn audit(_ctx: &PromptContext) -> String {
    "JOB (audit): run the maintainer audit on this worktree — apply `/good-to-go` \
     (doc sync, internal consistency, test coverage, build).\n\
     - If the audit surfaces real problems, file them as findings and send back: \
       `auwsx issue status \"$AUWSX_ISSUE_ID\" NEEDS_FIX`.\n\
     - If it passes: `auwsx issue status \"$AUWSX_ISSUE_ID\" ENDED`."
        .to_string()
}

fn conflicted(_ctx: &PromptContext) -> String {
    "JOB (resolve conflict): the merge of this issue's branch onto the current \
     default branch conflicts.\n\
     - Rebase the worktree branch onto the latest default branch and resolve \
       conflicts (NEVER merge the default branch into this one).\n\
     - On success: `auwsx issue status \"$AUWSX_ISSUE_ID\" COMPLETING`.\n\
     - If you cannot resolve it: `auwsx issue status \"$AUWSX_ISSUE_ID\" CONFLICT_BLOCKED`."
        .to_string()
}

fn completing(_ctx: &PromptContext) -> String {
    "JOB (complete): integrate this issue.\n\
     - Rebase the worktree branch onto the current default branch (do NOT merge \
       default into the branch), then merge with a single `--no-ff` commit.\n\
     - Record a memo of what shipped (`/memo`).\n\
     - On success: `auwsx issue status \"$AUWSX_ISSUE_ID\" DONE`.\n\
     - If the rebase hits conflicts you cannot finish here: \
       `auwsx issue status \"$AUWSX_ISSUE_ID\" CONFLICTED`."
        .to_string()
}
