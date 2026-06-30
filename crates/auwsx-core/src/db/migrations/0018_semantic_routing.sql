ALTER TABLE arsenal_agent_presets ADD COLUMN route_agent_cmd TEXT;
ALTER TABLE projects ADD COLUMN route_agent_cmd TEXT;

UPDATE arsenal_agent_presets
SET route_agent_cmd = CASE name
  WHEN 'codex' THEN 'codex exec --sandbox read-only --json {prompt}'
  WHEN 'claude' THEN 'claude --print --permission-mode bypassPermissions --output-format stream-json {prompt}'
  ELSE work_agent_cmd
END
WHERE route_agent_cmd IS NULL;

CREATE TABLE routing_runs (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  backlog_item_id INTEGER NOT NULL REFERENCES backlog_items(id) ON DELETE CASCADE,
  candidate_issue_ids TEXT NOT NULL,
  agent_cmd TEXT NOT NULL,
  prompt_path TEXT,
  log_path TEXT,
  raw_decision TEXT,
  parsed_decision TEXT,
  fallback_reason TEXT,
  exit_code INTEGER,
  exit_kind TEXT CHECK (exit_kind IS NULL OR exit_kind IN ('exited','timeout','killed','error')),
  spawned_at INTEGER NOT NULL,
  exited_at INTEGER
);

CREATE INDEX routing_runs_project ON routing_runs(project_id, id);
CREATE INDEX routing_runs_backlog_item ON routing_runs(backlog_item_id, id);
