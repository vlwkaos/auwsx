----------------------------------------------------------------------
-- remote repository integration
----------------------------------------------------------------------
CREATE TABLE project_remote_configs (
  project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
  provider TEXT NOT NULL DEFAULT 'github'
    CHECK (provider IN ('github')),
  remote_url TEXT NOT NULL,
  owner TEXT NOT NULL,
  repo TEXT NOT NULL,
  api_base_url TEXT NOT NULL DEFAULT 'https://api.github.com',
  auth_kind TEXT NOT NULL DEFAULT 'token_env'
    CHECK (auth_kind IN ('none','token_env','github_app')),
  auth_ref TEXT,
  webhook_secret_ref TEXT,
  inbound_auwsx_run_enabled INTEGER NOT NULL DEFAULT 0 CHECK (inbound_auwsx_run_enabled IN (0,1)),
  outbound_issue_create_enabled INTEGER NOT NULL DEFAULT 0 CHECK (outbound_issue_create_enabled IN (0,1)),
  remote_pr_merge_enabled INTEGER NOT NULL DEFAULT 0 CHECK (remote_pr_merge_enabled IN (0,1)),
  agent_comment_sync_enabled INTEGER NOT NULL DEFAULT 0 CHECK (agent_comment_sync_enabled IN (0,1)),
  subtask_comment_sync_enabled INTEGER NOT NULL DEFAULT 0 CHECK (subtask_comment_sync_enabled IN (0,1)),
  finding_comment_sync_enabled INTEGER NOT NULL DEFAULT 0 CHECK (finding_comment_sync_enabled IN (0,1)),
  draft_pr_enabled INTEGER NOT NULL DEFAULT 0 CHECK (draft_pr_enabled IN (0,1)),
  required_checks_policy TEXT NOT NULL DEFAULT 'observe'
    CHECK (required_checks_policy IN ('observe','require_green')),
  default_labels TEXT,
  default_assignees TEXT,
  pr_base_branch TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE INDEX project_remote_configs_repo
  ON project_remote_configs(provider, owner, repo);

CREATE TABLE remote_issue_links (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  issue_id INTEGER REFERENCES issues(id) ON DELETE CASCADE,
  backlog_item_id INTEGER REFERENCES backlog_items(id) ON DELETE CASCADE,
  provider TEXT NOT NULL CHECK (provider IN ('github')),
  remote_owner TEXT NOT NULL,
  remote_repo TEXT NOT NULL,
  remote_issue_number INTEGER NOT NULL,
  remote_node_id TEXT,
  remote_url TEXT NOT NULL,
  last_synced_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  CHECK (issue_id IS NOT NULL OR backlog_item_id IS NOT NULL),
  UNIQUE(provider, remote_owner, remote_repo, remote_issue_number),
  UNIQUE(issue_id),
  UNIQUE(backlog_item_id)
);

CREATE INDEX remote_issue_links_project
  ON remote_issue_links(project_id, updated_at);

CREATE TABLE remote_pr_links (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  provider TEXT NOT NULL CHECK (provider IN ('github')),
  remote_owner TEXT NOT NULL,
  remote_repo TEXT NOT NULL,
  remote_pr_number INTEGER NOT NULL,
  remote_node_id TEXT,
  remote_url TEXT NOT NULL,
  head_branch TEXT NOT NULL,
  head_sha TEXT,
  base_branch TEXT NOT NULL,
  base_sha TEXT,
  state TEXT NOT NULL DEFAULT 'open'
    CHECK (state IN ('open','closed','merged')),
  last_synced_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(provider, remote_owner, remote_repo, remote_pr_number),
  UNIQUE(issue_id)
);

CREATE INDEX remote_pr_links_project
  ON remote_pr_links(project_id, state, updated_at);

CREATE TABLE remote_events (
  id INTEGER PRIMARY KEY,
  project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
  provider TEXT NOT NULL CHECK (provider IN ('github')),
  delivery_id TEXT NOT NULL,
  event_kind TEXT NOT NULL,
  action TEXT,
  payload_hash TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'received'
    CHECK (status IN ('received','processed','ignored','failed')),
  error TEXT,
  received_at INTEGER NOT NULL,
  processed_at INTEGER,
  UNIQUE(provider, delivery_id)
);

CREATE INDEX remote_events_project
  ON remote_events(project_id, received_at);

CREATE TABLE remote_sync_runs (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  issue_id INTEGER REFERENCES issues(id) ON DELETE CASCADE,
  backlog_item_id INTEGER REFERENCES backlog_items(id) ON DELETE CASCADE,
  remote_issue_link_id INTEGER REFERENCES remote_issue_links(id) ON DELETE SET NULL,
  remote_pr_link_id INTEGER REFERENCES remote_pr_links(id) ON DELETE SET NULL,
  direction TEXT NOT NULL CHECK (direction IN ('inbound','outbound')),
  kind TEXT NOT NULL CHECK (kind IN ('webhook','issue','comment','pr')),
  status TEXT NOT NULL CHECK (status IN ('queued','running','done','failed','skipped')),
  summary TEXT,
  error TEXT,
  started_at INTEGER,
  ended_at INTEGER,
  created_at INTEGER NOT NULL
);

CREATE INDEX remote_sync_runs_project
  ON remote_sync_runs(project_id, created_at);
