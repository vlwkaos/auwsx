---
name: commit
description: Create smart, organized git commits from current changes
allowed-tools: Bash(git:*), Read, Grep
---

# Smart Commit

Create commits based on ACTUAL diff content, not assumptions.

## Workflow

### 1. Read Changes (MANDATORY)

```bash
git status && git branch --show-current
git diff --cached --stat && git diff --cached  # staged
git diff --stat && git diff                     # unstaged
```

### 2. Group + Commit

Group by purpose/module (1-8 files per commit), then commit immediately:

```bash
git add <files>
git diff --cached --stat  # verify staged
git commit -m "<type>(<scope>): [TICKET] <description>

<body>"
git log -1 --stat         # verify commit
```

> **Note**: Use `dangerouslyDisableSandbox: true` on all `git commit` Bash calls — lefthook pre-commit hooks require npm/filesystem access blocked by sandbox.

### 3. Report

After all commits, summarize: commit hash, subject, files changed.

## Format

```
<type>(<scope>): [TICKET] <description>

<body>
```

- **Types**: feat, fix, refactor, docs, chore, test, skill
- **Ticket**: Extract from branch (e.g., `feature/TICKET-123` → `[TICKET-123]`)
- **Subject**: Start with verb, max 72 chars, no period — be specific (e.g., `add mini.align for text alignment`, not `add plugin`)
- **Body**: 1-2 lines max. State *what* changed and *why* if non-obvious. Omit if subject is fully self-explanatory.

## Rules

- Read diff before EVERY commit message
- Base messages on observed changes, not file names
- DO NOT push automatically
- DO NOT commit secrets (.env, credentials)
- Large changeset (>20 files): create smaller commits
- No ticket in branch: omit ticket reference
