UPDATE projects
SET max_concurrency = 3
WHERE max_concurrency = 1;
