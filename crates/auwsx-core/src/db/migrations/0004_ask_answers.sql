----------------------------------------------------------------------
-- ask_answers (operator ask-mode history)
----------------------------------------------------------------------
CREATE TABLE ask_answers (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  mode TEXT NOT NULL CHECK (mode IN ('recall','seek')),
  question TEXT NOT NULL,
  answer TEXT NOT NULL,
  context_summary TEXT,
  log_path TEXT,
  created_at INTEGER NOT NULL
);

CREATE INDEX ask_answers_project_created ON ask_answers(project_id, created_at DESC);
