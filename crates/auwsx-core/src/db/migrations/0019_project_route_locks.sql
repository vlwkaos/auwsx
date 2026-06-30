CREATE TABLE project_route_locks (
  project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
  acquired_at INTEGER NOT NULL
);
