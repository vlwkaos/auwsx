---
name: keep-my-secret
description: Scans codebase for hardcoded secrets, committed credentials, and .env exposure. Use when auditing a project before push, reviewing a new repo, or after a suspected leak.
allowed-tools: Bash
argument-hint: "[path]"
---

# keep-my-secret

Scans for hardcoded secrets, committed credentials, and .env exposure.

## Usage

```bash
bash ~/.claude/skills/keep-my-secret/scripts/scan-secrets.sh [path]
```

Default path: current working directory.

Exit codes: `0` = clean, `1` = warnings, `2` = secrets found (rotate immediately).

## What it checks

| Layer | Checks |
|-------|--------|
| Dedicated tools | `gitleaks` (if installed), `trufflehog` (if installed) |
| Regex patterns | AWS keys, GitHub PATs, private key headers, Slack/Stripe/Twilio/SendGrid tokens, DB URL passwords, generic `secret=` / `password=` / `api_key=` assignments |
| Git-tracked files | `.env`, `.pem`, `.key`, `.p12`, `id_rsa`, etc. tracked by git |
| .env gitignore hygiene | Warns if `.env`, `*.pem`, `*.key`, `secrets/` not in `.gitignore` |
| Git history | Scans last 500 commits for ever-committed sensitive files (even if since deleted) |

## Workflow

1. Run the scan: `bash ~/.claude/skills/keep-my-secret/scripts/scan-secrets.sh [path]`
2. On exit 2: rotate any exposed credentials immediately before fixing the code
3. For history hits (`[FAIL]` in git history section): purge with `git-filter-repo` or BFG
4. For `[WARN]` on missing tools: `brew install gitleaks trufflehog` for deeper coverage

## False positives

The generic `secret=` pattern has moderate false positive rate on test fixtures and example configs. Review hits in context before rotating.

## Remediation

```bash
# Remove secret from git history
brew install git-filter-repo
git filter-repo --path path/to/secret-file --invert-paths

# Add to .gitignore
echo '.env' >> .gitignore
echo '*.pem' >> .gitignore
echo '*.key' >> .gitignore
```
