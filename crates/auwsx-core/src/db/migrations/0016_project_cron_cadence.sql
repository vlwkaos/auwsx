ALTER TABLE projects ADD COLUMN schedule_cron TEXT;
ALTER TABLE projects ADD COLUMN deepsleep_cron TEXT DEFAULT '0 0 * * 0';

UPDATE projects
SET schedule_cron = CASE
    WHEN schedule_interval_min IS NULL THEN NULL
    WHEN schedule_interval_min <= 0 THEN '@tick'
    WHEN schedule_interval_min % 1440 = 0 THEN '0 0 */' || (schedule_interval_min / 1440) || ' * *'
    WHEN schedule_interval_min % 60 = 0 THEN '0 */' || (schedule_interval_min / 60) || ' * * *'
    ELSE '@every ' || schedule_interval_min || 'm'
END
WHERE schedule_cron IS NULL;

UPDATE projects
SET deepsleep_cron = CASE
    WHEN deepsleep_interval_days <= 0 THEN NULL
    WHEN deepsleep_interval_days = 1 THEN '0 0 * * *'
    WHEN deepsleep_interval_days = 7 THEN '0 0 * * 0'
    ELSE '0 0 */' || deepsleep_interval_days || ' * *'
END;
