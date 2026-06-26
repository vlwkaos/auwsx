---
name: gh-pr
description: Create GitHub PR with auto-generated title/body and issue linking
allowed-tools: Bash, Read
---

# GitHub PR Create

## Workflow

1. Read commits and diff to understand what changed:
   ```bash
   git log --oneline -10
   git diff --stat main...HEAD
   ```

2. Generate:
   - **Title**: verb-first, conventional commit style, max 72 chars
   - **Body**: Korean, 2-4 bullets, 5-10 words each
     ```
     ## 변경사항
     - 항목1
     - 항목2
     ```

3. Run:
   ```bash
   bash ~/.claude/skills/gh-pr/scripts/gh-pr.sh \
     --title "<title>" \
     --body "<body>" \
     [--base <branch>] [--draft]
   ```
   Script handles: push, `Closes #N` appending, PR creation, URL output.

## Pitfall: Shell escaping with multiline bodies

Never pass a multiline PR body as an inline shell string — backticks, `$()`, and special chars cause parse errors. Write to a temp file instead:

```bash
cat > /tmp/pr_body.md << 'EOF'
## What does this PR do?
...
EOF

gh pr create --title "..." --body-file /tmp/pr_body.md
```

When creating PRs to upstream repos without push access, fork first:
```bash
gh repo fork <owner>/<repo> --clone=false
git remote add fork git@github.com:<user>/<repo>.git
git push fork <branch>
gh pr create --repo <owner>/<repo> --head <user>:<branch> --base main --body-file /tmp/pr_body.md
```
