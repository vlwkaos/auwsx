-- Rename issue statuses to operator-oriented lifecycle names and add ABANDONED.
-- SQLite cannot alter a CHECK domain in place, so rebuild the table.

CREATE TABLE issues_new (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  title TEXT NOT NULL,
  description TEXT,
  status TEXT NOT NULL CHECK (status IN (
    'NEW','PLANNING','PLAN_READY','PLAN_BLOCKED',
    'WORKING','REVIEWING','FIXING','REVIEW_BLOCKED','AUDITING',
    'READY_TO_MERGE','MERGING','RESOLVING_CONFLICT','CONFLICT_BLOCKED',
    'DONE','FAILED','ABANDONED'
  )),
  branch TEXT,
  worktree_path TEXT,
  agent_session TEXT,
  review_round INTEGER NOT NULL DEFAULT 0,
  conflict_attempts INTEGER NOT NULL DEFAULT 0,
  wait_until INTEGER,
  absorbed_into_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,
  has_pending_steering INTEGER NOT NULL DEFAULT 0 CHECK (has_pending_steering IN (0,1)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

INSERT INTO issues_new (
  id, project_id, title, description, status, branch, worktree_path, agent_session,
  review_round, conflict_attempts, wait_until, absorbed_into_id, has_pending_steering,
  created_at, updated_at
)
SELECT
  id,
  project_id,
  title,
  description,
  CASE status
    WHEN 'CONSOLIDATING' THEN 'NEW'
    WHEN 'PLANNED' THEN 'PLAN_READY'
    WHEN 'IMPLEMENTING' THEN 'WORKING'
    WHEN 'REVIEW' THEN 'REVIEWING'
    WHEN 'NEEDS_FIX' THEN 'FIXING'
    WHEN 'AUDIT' THEN 'AUDITING'
    WHEN 'ENDED' THEN 'READY_TO_MERGE'
    WHEN 'COMPLETING' THEN 'MERGING'
    WHEN 'CONFLICTED' THEN 'RESOLVING_CONFLICT'
    WHEN 'ABSORBED' THEN 'ABANDONED'
    ELSE status
  END,
  branch,
  worktree_path,
  agent_session,
  review_round,
  conflict_attempts,
  wait_until,
  absorbed_into_id,
  has_pending_steering,
  created_at,
  updated_at
FROM issues;

DROP TABLE issues;
ALTER TABLE issues_new RENAME TO issues;
CREATE INDEX issues_project_status ON issues(project_id, status);

