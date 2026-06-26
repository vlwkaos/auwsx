CREATE TABLE memory_presets (
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL CHECK (length(trim(name)) > 0),
  retrieve_kind TEXT NOT NULL CHECK (retrieve_kind IN ('portable','command','auwsx_skill')),
  retrieve_cmd TEXT,
  save_kind TEXT NOT NULL CHECK (save_kind IN ('portable','command','auwsx_skill')),
  save_cmd TEXT,
  dream_kind TEXT NOT NULL CHECK (dream_kind IN ('portable','command','auwsx_skill')),
  dream_cmd TEXT,
  deepsleep_kind TEXT NOT NULL CHECK (deepsleep_kind IN ('portable','command','auwsx_skill')),
  deepsleep_cmd TEXT,
  builtin INTEGER NOT NULL DEFAULT 0 CHECK (builtin IN (0,1)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

INSERT INTO memory_presets
  (name, retrieve_kind, retrieve_cmd, save_kind, save_cmd, dream_kind, dream_cmd,
   deepsleep_kind, deepsleep_cmd, builtin, created_at, updated_at)
VALUES
  (
    'portable-markdown',
    'portable',
    NULL,
    'portable',
    NULL,
    'portable',
    NULL,
    'portable',
    NULL,
    1,
    0,
    0
  ),
  (
    'auwsx-skills',
    'command',
    'bash {skill_root}/seek/scripts/seek.sh {query}',
    'auwsx_skill',
    NULL,
    'auwsx_skill',
    NULL,
    'auwsx_skill',
    NULL,
    1,
    0,
    0
  )
ON CONFLICT(name) DO NOTHING;

ALTER TABLE global_settings
ADD COLUMN memory_preset_name TEXT NOT NULL DEFAULT 'portable-markdown';
