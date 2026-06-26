-- Global profiles group projects in the TUI. Moving a project to a profile
-- appends it to that profile by assigning the next profile_order.

CREATE TABLE profiles (
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  ord INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

INSERT INTO profiles (id, name, ord, created_at)
VALUES (1, 'Default', 0, CAST(strftime('%s','now') AS INTEGER) * 1000);

ALTER TABLE projects ADD COLUMN profile_id INTEGER REFERENCES profiles(id) ON DELETE RESTRICT;
ALTER TABLE projects ADD COLUMN profile_order INTEGER NOT NULL DEFAULT 0;

UPDATE projects
SET profile_id = 1,
    profile_order = id
WHERE profile_id IS NULL OR profile_order = 0;

CREATE INDEX projects_profile_order ON projects(profile_id, profile_order, id);
