-- Backlog routing now records grouping on backlog_items.consumed_issue_id.
ALTER TABLE issues DROP COLUMN absorbed_into_id;
