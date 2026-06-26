CREATE TABLE arsenal_agent_presets (
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL CHECK (length(trim(name)) > 0),
  main_agent_cmd TEXT NOT NULL CHECK (length(trim(main_agent_cmd)) > 0),
  plan_agent_cmd TEXT NOT NULL CHECK (length(trim(plan_agent_cmd)) > 0),
  work_agent_cmd TEXT NOT NULL CHECK (length(trim(work_agent_cmd)) > 0),
  review_agent_cmd TEXT,
  builtin INTEGER NOT NULL DEFAULT 0 CHECK (builtin IN (0,1)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

INSERT INTO arsenal_agent_presets
  (name, main_agent_cmd, plan_agent_cmd, work_agent_cmd, review_agent_cmd, builtin, created_at, updated_at)
VALUES
  (
    'codex',
    'codex exec --sandbox workspace-write --json {prompt}',
    'codex exec --sandbox workspace-write --json {prompt}',
    'codex exec --sandbox workspace-write --json {prompt}',
    'codex exec --sandbox workspace-write --json {prompt}',
    1,
    0,
    0
  ),
  (
    'claude',
    'claude --print --permission-mode bypassPermissions --output-format stream-json {prompt}',
    'claude --print --permission-mode bypassPermissions --output-format stream-json {prompt}',
    'claude --print --permission-mode bypassPermissions --output-format stream-json {prompt}',
    'claude --print --permission-mode bypassPermissions --output-format stream-json {prompt}',
    1,
    0,
    0
  )
ON CONFLICT(name) DO NOTHING;
