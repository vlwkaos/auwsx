#!/usr/bin/env bash
# gather.sh — collect raw audit data for good-to-go
# Usage: gather.sh [commit|branch|all]
set -euo pipefail

SCOPE="${1:-default}"

# step 0: scope
echo "=== SCOPE ==="
git status --short
case "$SCOPE" in
  commit) git log --oneline -1 ;;
  branch) git log --oneline "$(git merge-base HEAD origin/HEAD)"..HEAD ;;
  all)    git log --oneline -20 ;;
  *)      git log --oneline "origin/HEAD..HEAD" 2>/dev/null || git log --oneline -10 ;;
esac

# step 1: release history patterns
echo "=== RELEASE HISTORY ==="
TAGS=$(git tag --sort=-creatordate | head -3)
echo "$TAGS"
for TAG in $TAGS; do
  echo "--- $TAG ---"
  git log --oneline "${TAG}~3..${TAG}" --name-only 2>/dev/null | head -20 || true
done
grep -A 20 "## Release\|## good-to-go\|## pre-release" AGENTS.md 2>/dev/null || true
echo "=== PROJECT AUDIT AXES ==="
cat knowledge/coding/good-to-go-axes.md 2>/dev/null || echo "(none — see AGENTS.md ## good-to-go fallback)"

# step 2: docs
echo "=== DOCS ==="
rg --files . 2>/dev/null | rg -i "readme|changelog|changes|history" | sort
rg --files . 2>/dev/null | rg '\.[a-z]{2}\.(md|rst|txt)$' | sort

# step 3: changelog
echo "=== CHANGELOG ==="
head -30 CHANGELOG.md 2>/dev/null || echo "no CHANGELOG"

# step 5: build + test — discover available commands, don't assume toolchain
echo "=== BUILD ==="
if [ -f Cargo.toml ]; then
  cargo build --all-targets 2>&1 | tail -5
  cargo test 2>&1 | tail -10
  cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
elif [ -f package.json ]; then
  echo "--- available scripts ---"
  node -e "const p=require('./package.json'); console.log(Object.keys(p.scripts||{}).join('\n'))" 2>/dev/null || true
  echo "--- type-check ---"
  for CMD in type-check typecheck check; do
    node -e "const p=require('./package.json'); process.exit(p.scripts?.['$CMD']?0:1)" 2>/dev/null && pnpm run "$CMD" 2>&1 | tail -8 && break
  done || echo "(no type-check script)"
  echo "--- test ---"
  pnpm test 2>&1 | tail -10
elif [ -f Makefile ]; then
  make test 2>&1 | tail -10
fi
