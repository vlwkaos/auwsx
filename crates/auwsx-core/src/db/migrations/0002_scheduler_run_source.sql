ALTER TABLE scheduler_runs
  ADD COLUMN source TEXT NOT NULL DEFAULT 'auto' CHECK (source IN ('auto', 'manual'));

CREATE INDEX scheduler_runs_project_source ON scheduler_runs(project_id, source, fired_at);
