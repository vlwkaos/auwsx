ALTER TABLE projects ADD COLUMN arsenal_preset_name TEXT
  REFERENCES arsenal_agent_presets(name) ON UPDATE CASCADE ON DELETE SET NULL;
