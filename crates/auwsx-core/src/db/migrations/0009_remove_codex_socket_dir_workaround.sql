UPDATE projects
SET main_agent_cmd = 'codex exec --sandbox workspace-write --json {prompt}'
WHERE main_agent_cmd LIKE 'codex exec --sandbox workspace-write --add-dir %/Library/Caches/auwsx --json {prompt}';

UPDATE projects
SET plan_agent_cmd = 'codex exec --sandbox workspace-write --json {prompt}'
WHERE plan_agent_cmd LIKE 'codex exec --sandbox workspace-write --add-dir %/Library/Caches/auwsx --json {prompt}';

UPDATE projects
SET work_agent_cmd = 'codex exec --sandbox workspace-write --json {prompt}'
WHERE work_agent_cmd LIKE 'codex exec --sandbox workspace-write --add-dir %/Library/Caches/auwsx --json {prompt}';

UPDATE projects
SET review_agent_cmd = 'codex exec --sandbox workspace-write --json {prompt}'
WHERE review_agent_cmd LIKE 'codex exec --sandbox workspace-write --add-dir %/Library/Caches/auwsx --json {prompt}';
