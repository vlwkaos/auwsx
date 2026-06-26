#!/usr/bin/env bash
# gh-pr.sh — push branch and create GitHub PR
# Usage: gh-pr.sh --title <title> --body <body> [--base <branch>] [--draft]
set -euo pipefail

BASE="main"
DRAFT=""
TITLE=""
BODY=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)  BASE="$2";  shift 2 ;;
    --draft) DRAFT="--draft"; shift ;;
    --title) TITLE="$2"; shift 2 ;;
    --body)  BODY="$2";  shift 2 ;;
    *) echo "Unknown arg: $1" >&2; exit 2 ;;
  esac
done

[[ -z "$TITLE" ]] && read -rp "PR title: " TITLE
[[ -z "$BODY"  ]] && { echo "PR body (Ctrl-D to end):"; BODY=$(cat); }

TICKET=$(git rev-parse --abbrev-ref HEAD | grep -oE '#[0-9]+' | head -1 || true)
if [[ -n "$TICKET" ]] && ! echo "$BODY" | grep -q "Closes"; then
  BODY="${BODY}"$'\n\n'"Closes ${TICKET}"
fi

git push -u origin HEAD
gh pr create --title "$TITLE" --body "$BODY" --base "$BASE" $DRAFT
