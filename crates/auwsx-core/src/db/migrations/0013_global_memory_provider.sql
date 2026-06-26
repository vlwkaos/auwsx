ALTER TABLE global_settings
ADD COLUMN memory_provider TEXT NOT NULL DEFAULT 'portable-markdown'
  CHECK (memory_provider IN ('portable-markdown', 'auwsx-skills'));
