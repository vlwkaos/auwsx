#!/usr/bin/env bash
# note-persist.sh — index and commit a newly written vault note
# Usage: note-persist.sh <vault_root> <collection> <canonical_title>
set -euo pipefail

VAULT_ROOT="${1:-}"
COLLECTION="${2:-}"
CANONICAL_TITLE="${3:-}"

if [[ -z "$VAULT_ROOT" || -z "$COLLECTION" || -z "$CANONICAL_TITLE" ]]; then
    echo "Usage: note-persist.sh <vault_root> <collection> <canonical_title>" >&2
    exit 2
fi

# step 1: update ir index and embed
ir update "$COLLECTION" && ir embed "$COLLECTION"

# step 2: stage all notes changes
git -C "$VAULT_ROOT" add notes/

# step 3: commit if staged changes exist
git -C "$VAULT_ROOT" diff --cached --quiet \
    || git -C "$VAULT_ROOT" commit -m "note: $CANONICAL_TITLE - $(date +%Y-%m-%d)"

# step 4: push
git -C "$VAULT_ROOT" push
