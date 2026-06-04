-- auwsx initial schema. Plan Step 6.
-- All timestamps are Unix epoch milliseconds (INTEGER).

CREATE TABLE projects (
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  repo_path TEXT NOT NULL,
  default_branch TEXT NOT NULL,
  agent TEXT NOT NULL,            -- claude | codex | opencode
  schedule_interval_min INTEGER,  -- NULL = manual only
  max_concurrency INTEGER NOT NULL DEFAULT 1,
  merge_mode TEXT NOT NULL DEFAULT 'auto', -- auto | pr | local
  deepsleep_interval_days INTEGER NOT NULL DEFAULT 7,
  last_deepsleep_at INTEGER,
  created_at INTEGER NOT NULL
);

CREATE TABLE tasks (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  title TEXT NOT NULL,
  description TEXT,
  status TEXT NOT NULL,           -- BACKLOG | QUEUED | PREPARING | ITERATING |
                                  -- QA | PENDING_FEEDBACK | READY | COMPLETING |
                                  -- KNOWLEDGE_PROPAGATING | DONE | FAILED
  iteration INTEGER NOT NULL DEFAULT 0,
  branch TEXT,                    -- assigned at PREPARING
  worktree_path TEXT,
  agent_session TEXT,
  shell_session TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE INDEX tasks_project_status ON tasks(project_id, status);

CREATE TABLE iterations (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  n INTEGER NOT NULL,
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  exit_kind TEXT,                 -- signal-done | process-exit | timeout | error
  log_path TEXT
);

CREATE INDEX iterations_task ON iterations(task_id);

CREATE TABLE feedback (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  iteration INTEGER NOT NULL,
  body TEXT NOT NULL,
  submitted_at INTEGER NOT NULL
);

CREATE TABLE scheduler_runs (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  fired_at INTEGER NOT NULL,
  picked_task_ids TEXT             -- JSON array
);

CREATE TABLE drafts (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  body TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending',  -- pending | consumed | discarded
  consumed_by TEXT,                        -- "task:{id}" | "merge:{task_id}" | "split:{task_id,...}"
  discard_reason TEXT,
  created_at INTEGER NOT NULL,
  resolved_at INTEGER
);

CREATE INDEX drafts_project_state ON drafts(project_id, state);

CREATE TABLE followups (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  body TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  consumed_at INTEGER,            -- NULL = pending; set when concatenated into a feedback file
  consumed_into_iter INTEGER
);

CREATE INDEX followups_task_pending ON followups(task_id, consumed_at);

CREATE TABLE routines (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,             -- "daily-news", "deepsleep", "dream", ...
  origin TEXT NOT NULL,           -- builtin | user
  prompt_template TEXT NOT NULL,  -- agent prompt; may contain {output}, {date}, {datetime}
  cron TEXT NOT NULL,             -- cron expression
  enabled INTEGER NOT NULL DEFAULT 1,
  output_target TEXT,             -- repo-relative path template; NULL = log only
  last_run_at INTEGER,
  next_run_at INTEGER,             -- maintained by scheduler
  created_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);

CREATE TABLE main_jobs (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  routine_id INTEGER REFERENCES routines(id) ON DELETE SET NULL,  -- NULL for post_merge / user_oneoff
  source TEXT NOT NULL,           -- post_merge | routine | user_oneoff
  kind TEXT NOT NULL,             -- dream | deepsleep | release | memo | custom | triage
  prompt TEXT NOT NULL,           -- materialized prompt actually sent to agent
  status TEXT NOT NULL,           -- QUEUED | RUNNING | DONE | FAILED
  started_at INTEGER,
  ended_at INTEGER,
  log_path TEXT,
  outcome TEXT                    -- one-line summary
);

CREATE INDEX main_jobs_project_status ON main_jobs(project_id, status);
