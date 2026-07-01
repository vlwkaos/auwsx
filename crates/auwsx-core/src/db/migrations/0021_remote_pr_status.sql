ALTER TABLE remote_pr_links
  ADD COLUMN check_status TEXT NOT NULL DEFAULT 'unknown'
    CHECK (check_status IN ('unknown','pending','success','failure'));

ALTER TABLE remote_pr_links
  ADD COLUMN check_summary TEXT;

ALTER TABLE remote_pr_links
  ADD COLUMN merge_state_status TEXT;

ALTER TABLE remote_pr_links
  ADD COLUMN review_decision TEXT;
