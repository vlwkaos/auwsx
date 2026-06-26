#!/usr/bin/env bash
# deepsleep-audit.sh — emit structured KEY=VALUE audit lines for a knowledge/ tree.
# Usage: deepsleep-audit.sh <knowledge_dir>
# Exit 0 on success; non-zero on bad input.
# Output is for a downstream LLM workflow (deepsleep SKILL.md) to parse.

set -euo pipefail

KDIR="${1:-}"
[ -d "$KDIR" ] || { echo "ERROR: not a directory: $KDIR" >&2; exit 1; }

RG="$(command -v rg || echo /opt/homebrew/bin/rg)"
[ -x "$RG" ] || { echo "ERROR: ripgrep not found" >&2; exit 1; }

# --- Inventory ----------------------------------------------------------------

FILE_COUNT=$("$RG" --files "$KDIR" -t md 2>/dev/null | wc -l | tr -d ' ')
TOTAL_BYTES=$(du -sk "$KDIR" 2>/dev/null | awk '{print $1*1024}')
PLAN_LINES=0
[ -f "$KDIR/plan.md" ] && PLAN_LINES=$(wc -l < "$KDIR/plan.md" | tr -d ' ')

echo "INVENTORY_FILES=$FILE_COUNT"
echo "INVENTORY_BYTES=$TOTAL_BYTES"
echo "INVENTORY_PLAN_LINES=$PLAN_LINES"

# --- impl-* in coding/ or domain/ --------------------------------------------

while IFS= read -r f; do
  echo "IMPL_BLOAT=$f"
done < <(ls "$KDIR/coding/impl-"*.md "$KDIR/domain/impl-"*.md 2>/dev/null || true)

# --- Oversize files (>250 lines OR >12K bytes) -------------------------------

while IFS= read -r f; do
  lines=$(wc -l < "$f" | tr -d ' ')
  bytes=$(wc -c < "$f" | tr -d ' ')
  if [ "$lines" -gt 250 ] || [ "$bytes" -gt 12288 ]; then
    echo "OVERSIZE=$f lines=$lines bytes=$bytes"
  fi
done < <("$RG" --files "$KDIR/coding" "$KDIR/domain" -t md 2>/dev/null)

# --- Front-matter completeness (slug, kind, title, description, keywords ≥6) -

while IFS= read -r f; do
  head=$(awk '/^---$/{c++;next} c==1{print} c==2{exit}' "$f")
  for field in slug kind title description keywords; do
    if ! echo "$head" | grep -qE "^${field}:"; then
      echo "MISSING_FRONTMATTER=$f field=$field"
    fi
  done
  # keyword count
  kw_line=$(echo "$head" | grep -E '^keywords:' | head -1 | sed -E 's/^keywords:[[:space:]]*//')
  if [ -n "$kw_line" ]; then
    kw_count=$(echo "$kw_line" | tr ',' '\n' | sed '/^[[:space:]]*$/d' | wc -l | tr -d ' ')
    [ "$kw_count" -lt 6 ] && echo "THIN_KEYWORDS=$f count=$kw_count"
  fi
done < <("$RG" --files "$KDIR/coding" "$KDIR/domain" -t md 2>/dev/null)

# --- Self-superseded markers --------------------------------------------------

while IFS= read -r f; do
  echo "SELF_SUPERSEDED=$f"
done < <("$RG" -l '^Replaces |^Superseded|^Deprecated' "$KDIR/coding" "$KDIR/domain" 2>/dev/null || true)

# --- Broken code anchors (anchor → missing slug file) ------------------------

# Find packages/ dir relative to knowledge/ parent
PROJ_ROOT="$(dirname "$KDIR")"
PKG_DIR="$PROJ_ROOT/packages"
SRC_DIR="$PROJ_ROOT/src"
SCAN=""
[ -d "$PKG_DIR" ] && SCAN="$PKG_DIR"
[ -d "$SRC_DIR" ] && SCAN="$SCAN $SRC_DIR"

if [ -n "$SCAN" ]; then
  # rg exits 1 when a project's src/ has no [[anchor]] markers — that is a
  # valid "clean" result, not an error. `|| true` keeps pipefail from aborting
  # the whole audit before STALE_REF / SLUG_COLLISION run.
  { "$RG" --no-filename --no-line-number -o '\[\[[a-z][a-z0-9+_-]+\]\]' $SCAN 2>/dev/null || true; } \
    | sort -u \
    | while IFS= read -r anchor; do
        [ -z "$anchor" ] && continue
        slug="${anchor#[[}"; slug="${slug%]]}"
        if [ ! -f "$KDIR/coding/$slug.md" ] && [ ! -f "$KDIR/domain/$slug.md" ]; then
          echo "BROKEN_ANCHOR=$anchor"
        fi
      done
fi

# --- Stale internal refs (knowledge/ files mentioning impl-*.md paths) -------

"$RG" -l 'impl-[a-z][a-z0-9-]*\.md' "$KDIR" 2>/dev/null \
  | while IFS= read -r f; do
      echo "STALE_REF=$f"
    done || true

# --- Slug-name collisions across kinds (same basename in coding+domain+history)

basenames=$("$RG" --files "$KDIR" -t md 2>/dev/null | xargs -n1 basename | sort | uniq -c | awk '$1 > 1 {print $2}')
if [ -n "$basenames" ]; then
  echo "$basenames" | while IFS= read -r b; do
    [ -z "$b" ] && continue
    echo "SLUG_COLLISION=$b"
  done
fi

exit 0
