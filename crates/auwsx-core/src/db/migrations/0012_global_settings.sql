CREATE TABLE global_settings (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  pipeline_ux_guidance TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);

INSERT INTO global_settings (id, pipeline_ux_guidance, updated_at)
VALUES (
  1,
  'Build auwsx as an operator console. Derive visible actions from current capabilities, preserve focus/return context, use typed controls for closed domains, avoid duplicate paths, handle invalid/terminal states explicitly, and cover failure/restoration paths instead of only happy paths.',
  0
);
