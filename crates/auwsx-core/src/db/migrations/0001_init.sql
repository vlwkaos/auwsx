-- auwsx schema. Design: ~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md (revised model).
-- All timestamps are Unix epoch milliseconds (INTEGER).
--
-- Model summary (decisions ledger):
--   * Issue status is the synchronization marker. Scheduler tick reads status:
--     ACTIONABLE -> spawn agent, HUMAN_GATED -> wait, TERMINAL -> archive.
--     The canonical status domain + classes live in src/state.rs::IssueStatus;
--     the CHECK below MUST match it exactly.
--   * backlog_items replaces drafts (source + approval gate).
--   * steering replaces followups (append-only, never edits plan).
--   * Per-role agent commands (main/plan/work/review) + configurable policies.
--   * agent_runs = append-only action log (transparency + self-eval data).
--
-- CHECK domains are intentional: inputs cross the SQLite boundary from IPC, so
-- the DB is the last line of validation, not the first.
--
-- FK enforcement is set per-connection in db/mod.rs (foreign_keys(true)); a
-- PRAGMA here would be a no-op inside sqlx's transactional migration.

----------------------------------------------------------------------
-- projects
----------------------------------------------------------------------
CREATE TABLE projects (
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  repo_path TEXT NOT NULL,
  default_branch TEXT NOT NULL,

  -- Per-role agent CLI command templates ({prompt} placeholder substituted at spawn).
  -- review_agent_cmd NULL => fall back to work_agent_cmd (fresh session still = third eye).
  main_agent_cmd  TEXT NOT NULL,
  plan_agent_cmd  TEXT NOT NULL,
  work_agent_cmd  TEXT NOT NULL,
  review_agent_cmd TEXT,

  -- Completion gate policy (ENDED -> COMPLETING).
  completion_policy TEXT NOT NULL DEFAULT 'manual'
    CHECK (completion_policy IN ('manual','soft','auto')),
  completion_soft_timeout_min INTEGER NOT NULL DEFAULT 60,  -- policy='soft' only

  -- Soft-gate window for PLANNED (human may intervene, else auto-advances).
  plan_gate_timeout_min INTEGER NOT NULL DEFAULT 10,

  -- Per-phase hard timeouts + loop caps.
  iteration_timeout_min INTEGER NOT NULL DEFAULT 30,
  main_job_timeout_min  INTEGER NOT NULL DEFAULT 60,
  review_max_rounds     INTEGER NOT NULL DEFAULT 5,
  conflict_max_attempts INTEGER NOT NULL DEFAULT 3,

  -- Concurrency: serial-per-project v1 (1 active issue / worktree at a time).
  max_concurrency INTEGER NOT NULL DEFAULT 1,
  schedule_interval_min INTEGER,            -- NULL = manual scheduler only

  -- Integration method (local = rebase + single --no-ff merge).
  merge_mode TEXT NOT NULL DEFAULT 'local'
    CHECK (merge_mode IN ('local','pr')),

  -- auwsx-owned skill path override; NULL = global auwsx default.
  skill_path TEXT,

  deepsleep_interval_days INTEGER NOT NULL DEFAULT 7,
  last_deepsleep_at INTEGER,
  created_at INTEGER NOT NULL
);

----------------------------------------------------------------------
-- routines (second lane: scheduled prompts, serialized via main queue)
----------------------------------------------------------------------
CREATE TABLE routines (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  origin TEXT NOT NULL CHECK (origin IN ('builtin','user')),
  -- write-scope class: report/idea never write files; knowledge writes only
  -- inside writable_paths (verified against the diff before auwsx commits).
  type TEXT NOT NULL CHECK (type IN ('report','idea','knowledge')),
  prompt TEXT NOT NULL,
  cron TEXT NOT NULL,
  writable_paths TEXT,                      -- JSON array of repo-relative globs; NULL for report/idea
  enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  last_run_at INTEGER,
  next_run_at INTEGER,                      -- maintained by scheduler
  created_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);

----------------------------------------------------------------------
-- issues (the pipeline unit; status = scheduler marker)
----------------------------------------------------------------------
CREATE TABLE issues (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  title TEXT NOT NULL,
  description TEXT,

  -- Must match src/state.rs::IssueStatus::as_str exactly.
  status TEXT NOT NULL CHECK (status IN (
    'CONSOLIDATING','PLANNING','PLANNED','IMPLEMENTING','REVIEW','NEEDS_FIX',
    'AUDIT','ENDED','COMPLETING','DONE',
    'PLAN_BLOCKED','REVIEW_BLOCKED','CONFLICTED','CONFLICT_BLOCKED',
    'ABSORBED','FAILED'
  )),

  -- Worktree is created at CONSOLIDATING->PLANNING (standalone issues only);
  -- delegated tasks fold into a target's worktree and never get their own.
  branch TEXT,
  worktree_path TEXT,
  agent_session TEXT,                       -- optional tmux session for human spectating

  -- Loop counters (caps enforced from projects.*).
  review_round INTEGER NOT NULL DEFAULT 0,
  conflict_attempts INTEGER NOT NULL DEFAULT 0,

  -- Soft-gate expiry (epoch ms). Set for PLANNED, and ENDED when policy='soft'.
  wait_until INTEGER,

  -- Set when this issue self-closed by delegating into another (status ABSORBED).
  absorbed_into_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,
  -- Re-trigger flag: flipped to 1 when new steering arrives for a working issue.
  has_pending_steering INTEGER NOT NULL DEFAULT 0 CHECK (has_pending_steering IN (0,1)),

  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE INDEX issues_project_status ON issues(project_id, status);

----------------------------------------------------------------------
-- subtasks (plan output; the IMPLEMENTING checklist)
----------------------------------------------------------------------
CREATE TABLE subtasks (
  id INTEGER PRIMARY KEY,
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  ord INTEGER NOT NULL,                     -- display / execution order
  text TEXT NOT NULL,
  done INTEGER NOT NULL DEFAULT 0 CHECK (done IN (0,1)),
  created_at INTEGER NOT NULL,
  done_at INTEGER
);

CREATE INDEX subtasks_issue ON subtasks(issue_id, ord);

----------------------------------------------------------------------
-- findings (reviewer output; drives REVIEW <-> NEEDS_FIX loop)
----------------------------------------------------------------------
CREATE TABLE findings (
  id INTEGER PRIMARY KEY,
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  review_round INTEGER NOT NULL,
  severity TEXT NOT NULL CHECK (severity IN ('blocker','major','minor','nit')),
  lens TEXT,                                -- correctness | simplicity | security | devils-advocate | backpressure | simplify
  title TEXT NOT NULL,
  detail TEXT,
  file_ref TEXT,
  -- open -> accepted|rejected by implementer adjudication; dismissed by human.
  status TEXT NOT NULL DEFAULT 'open'
    CHECK (status IN ('open','accepted','rejected','dismissed')),
  adjudication TEXT,                        -- implementer rationale, on the record
  created_at INTEGER NOT NULL,
  resolved_at INTEGER
);

CREATE INDEX findings_issue_round ON findings(issue_id, review_round);

----------------------------------------------------------------------
-- steering (replaces followups; append-only, never edits plan.md)
----------------------------------------------------------------------
CREATE TABLE steering (
  id INTEGER PRIMARY KEY,
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  source TEXT NOT NULL CHECK (source IN ('human','consolidation')),
  note TEXT NOT NULL,
  consumed INTEGER NOT NULL DEFAULT 0 CHECK (consumed IN (0,1)),
  created_at INTEGER NOT NULL,
  consumed_at INTEGER
);

CREATE INDEX steering_issue_pending ON steering(issue_id, consumed);

----------------------------------------------------------------------
-- backlog_items (replaces drafts; source + approval admission gate)
----------------------------------------------------------------------
CREATE TABLE backlog_items (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  text TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('human','agent','routine','inbox')),
  -- human/inbox-authored default to approved at insert time (app sets it);
  -- agent/routine-authored stay pending until a human approves/dismisses.
  approval TEXT NOT NULL DEFAULT 'pending'
    CHECK (approval IN ('pending','approved','dismissed')),
  origin_routine_id INTEGER REFERENCES routines(id) ON DELETE SET NULL,
  consumed_issue_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,  -- set when grouped
  created_at INTEGER NOT NULL,
  resolved_at INTEGER
);

CREATE INDEX backlog_project_approval ON backlog_items(project_id, approval);

----------------------------------------------------------------------
-- main_jobs (serialized main-branch queue: post_merge, routines, one-offs)
----------------------------------------------------------------------
CREATE TABLE main_jobs (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  routine_id INTEGER REFERENCES routines(id) ON DELETE SET NULL,  -- NULL for post_merge / one-off
  source TEXT NOT NULL CHECK (source IN ('post_merge','routine','user_oneoff')),
  kind TEXT NOT NULL,                       -- dream | deepsleep | memo | triage | report | idea | knowledge | custom
  prompt TEXT NOT NULL,
  status TEXT NOT NULL
    CHECK (status IN ('QUEUED','RUNNING','DONE','FAILED','REJECTED')),
  worktree_path TEXT,                       -- disposable for report/idea routines
  report_path TEXT,                         -- routine report artifact, surfaced after done
  scope_violation TEXT,                     -- set (-> REJECTED) when diff escaped writable_paths
  queued_at INTEGER NOT NULL,
  started_at INTEGER,
  ended_at INTEGER,
  log_path TEXT,
  outcome TEXT                              -- one-line summary
);

CREATE INDEX main_jobs_project_status ON main_jobs(project_id, status);

----------------------------------------------------------------------
-- agent_runs (append-only action log; transparency + self-eval data)
----------------------------------------------------------------------
CREATE TABLE agent_runs (
  id INTEGER PRIMARY KEY,
  -- Exactly one of issue_id / main_job_id is set (enforced in app layer).
  issue_id INTEGER REFERENCES issues(id) ON DELETE CASCADE,
  main_job_id INTEGER REFERENCES main_jobs(id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK (role IN ('main','plan','work','review')),
  phase TEXT NOT NULL,                      -- CONSOLIDATING | PLANNING | IMPLEMENTING | REVIEW | NEEDS_FIX | AUDIT | MEMO | COMPLETING | CONFLICTED | routine
  agent_cmd TEXT NOT NULL,
  status_before TEXT,
  status_after TEXT,
  pid INTEGER,
  exit_code INTEGER,
  exit_kind TEXT CHECK (exit_kind IS NULL OR exit_kind IN ('exited','timeout','killed','error')),
  prompt_path TEXT,
  log_path TEXT,
  spawned_at INTEGER NOT NULL,
  exited_at INTEGER,
  note TEXT
);

CREATE INDEX agent_runs_issue ON agent_runs(issue_id);
CREATE INDEX agent_runs_main_job ON agent_runs(main_job_id);

----------------------------------------------------------------------
-- scheduler_runs (observability: what each tick picked)
----------------------------------------------------------------------
CREATE TABLE scheduler_runs (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  fired_at INTEGER NOT NULL,
  picked TEXT                               -- JSON: {issues:[...], main_jobs:[...]}
);

CREATE INDEX scheduler_runs_project ON scheduler_runs(project_id, fired_at);
