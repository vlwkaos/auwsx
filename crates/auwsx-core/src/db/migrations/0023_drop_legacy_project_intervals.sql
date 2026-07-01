-- Cron fields are canonical after 0016_project_cron_cadence.sql copied legacy values.
ALTER TABLE projects DROP COLUMN schedule_interval_min;
ALTER TABLE projects DROP COLUMN deepsleep_interval_days;
