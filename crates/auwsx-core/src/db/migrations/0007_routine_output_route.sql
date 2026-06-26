-- Rename routine type semantics to output_route. Keep the old type column for
-- compatibility with existing DBs; new code reads output_route.

ALTER TABLE routines ADD COLUMN output_route TEXT NOT NULL DEFAULT 'log'
  CHECK (output_route IN ('log','queue','note'));

UPDATE routines
SET output_route = CASE type
  WHEN 'report' THEN 'log'
  WHEN 'idea' THEN 'queue'
  WHEN 'knowledge' THEN 'note'
  ELSE 'log'
END;

