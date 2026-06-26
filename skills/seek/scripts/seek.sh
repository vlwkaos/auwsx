#!/usr/bin/env bash
# seek.sh — lightweight KB lookup. Ergonomic tier with grep/Glob.
# Wraps `ir search` (collection-scoped) + `rg --files | rg` filename fallback.
# Returns numbered list with scores + one-line excerpts. Never reads files.
#
# Usage: seek.sh "<query>" [extra-collection ...]
set -o pipefail

QUERY="${1:-}"
[ -z "$QUERY" ] && { echo "usage: seek.sh <query> [extra-collection ...]" >&2; exit 2; }
shift

# ---------- project + vault detection ----------
git_root=$(git rev-parse --show-toplevel 2>/dev/null)
project_name=""
knowledge_dir=""
if [ -n "$git_root" ]; then
  origin=$(git -C "$git_root" remote get-url origin 2>/dev/null)
  if [ -n "$origin" ]; then
    project_name=$(basename "${origin%.git}")
  else
    project_name=$(basename "$git_root")
  fi
  [ -d "$git_root/knowledge" ] && knowledge_dir="$git_root/knowledge"
fi

vault_root=""
for c in "$HOME/ws/dgv3" "$HOME/ws-ps/dgv3" "$HOME/dgv3"; do
  [ -d "$c" ] && { vault_root="$c"; break; }
done

# ---------- collection list (newline-separated string, simpler than arrays) ----------
all_cols=$(ir collection ls 2>/dev/null | awk '{print $1}')
cols=""
add_col() { [ -z "$1" ] && return; echo "$all_cols" | grep -qx "$1" && cols="$cols$1
"; }

# ^ signal table mirrors ~/.claude/skills/ir/SKILL.md "Domain Collections".
#   Gaps to extend when needed: pyproject.toml→python, go.mod→go, Gemfile→ruby,
#   pom.xml→java, manifest.json→obsidian, monorepo subdirs (currently only repo root).
#   Always-relevant (git/terminal/ai) are NOT auto-added to avoid bloat — pass as
#   positional extra-collection args.
[ -n "$project_name" ] && add_col "$project_name"
if [ -n "$git_root" ]; then
  [ -f "$git_root/Cargo.toml" ] && add_col "rust"
  if [ -f "$git_root/package.json" ]; then
    add_col "typescript"
    grep -qE '"react"' "$git_root/package.json" 2>/dev/null && add_col "react"
    grep -qE '"svelte"' "$git_root/package.json" 2>/dev/null && add_col "svelte"
  fi
  [ -d "$git_root/.claude" ] && add_col "claude-code"
fi
for extra in "$@"; do add_col "$extra"; done

cols=$(printf '%s' "$cols" | awk 'NF && !seen[$0]++')

# ---------- run ir searches (parallel via xargs) ----------
# ^ Must template the path: macOS mktemp ignores $TMPDIR env and reads
#   confstr(_CS_DARWIN_USER_TEMP_DIR), which points outside the Claude sandbox
#   allowlist. Without this, ir search output is silently discarded.
tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/seek.XXXXXX")
trap 'rm -rf "$tmpdir"' EXIT

if [ -n "$cols" ]; then
  # ^ feed loop via <<<, not `echo | while`: a pipeline puts the loop
  #   in a subshell, and `wait` then misses backgrounded jobs → empty results.
  while IFS= read -r col; do
    [ -z "$col" ] && continue
    (
      ir search "$QUERY" -c "$col" -n 5 --min-score 0.15 2>/dev/null \
        | awk -v c="$col" '
            /^[0-9]+\.[0-9]+[[:space:]]+#[0-9a-f]+/ {
              if (path != "") printf("%s\tir:%s\t%s\t%s\n", score, c, path, excerpt)
              score=$1; path=$3; excerpt=""; lines=0; next
            }
            /^[[:space:]]+/ {
              if (excerpt == "" && lines < 4) {
                line=$0; sub(/^[[:space:]]+/,"",line)
                sub(/^\.\.\./,"",line); sub(/^---$/,"",line)
                if (line != "" && line !~ /^(title|slug|kind|description|created|modified|keywords|target_slugs):/) excerpt=line
                lines++
              }
              next
            }
            END { if (path != "") printf("%s\tir:%s\t%s\t%s\n", score, c, path, excerpt) }
          ' >> "$tmpdir/ir.out"
    ) &
  done <<< "$cols"
  wait
fi

# ---------- filename fallback ----------
search_terms=$(echo "$QUERY" | tr ' ' '\n' | awk 'length($0)>=3' | head -6 | paste -sd'|' -)
if [ -n "$search_terms" ]; then
  [ -n "$knowledge_dir" ] && \
    rg --files "$knowledge_dir" 2>/dev/null | rg -i "$search_terms" 2>/dev/null \
      | awk '{printf("0.10\tfilename\t%s\t(filename match)\n", $0)}' >> "$tmpdir/ir.out"
  if [ -n "$vault_root" ]; then
    for d in "$vault_root/notes/knowledges" "$vault_root/notes-local/knowledges"; do
      [ -d "$d" ] && rg --files "$d" 2>/dev/null | rg -i "$search_terms" 2>/dev/null | head -5 \
        | awk '{printf("0.08\tvault-filename\t%s\t(filename match)\n", $0)}' >> "$tmpdir/ir.out"
    done
  fi
fi

# ---------- merge, dedupe by path, rank by score, display ----------
if [ -s "$tmpdir/ir.out" ]; then
  awk -F'\t' '$3 != "" && !seen[$3]++' "$tmpdir/ir.out" \
    | sort -t$'\t' -k1,1nr \
    | head -15 \
    | awk -F'\t' '{n++; printf("%d. %s  (%s %s)\n", n, $3, $2, $1); if ($4 != "") printf("   %s\n", $4)}'
else
  echo "(no results)"
fi

echo
cols_inline=$(echo "$cols" | tr '\n' ',' | sed 's/,$//')
echo "scope: project=${project_name:-none} collections=[${cols_inline}] vault=${vault_root:-none}"
echo "next: Read <path> to load; /recall <topic> for full planning context."

# ---------- self-bootstrap: spread /seek directive into project AGENTS.md ----------
# ^ first /seek invocation per project appends a one-line directive so future
#   agents discover the lookup reflex without re-reading global CLAUDE.md.
#   Idempotent: skips if either AGENTS.md or CLAUDE.md already mentions /seek.
#   Skips symlinks (avoids contaminating global CLAUDE.md via project symlink).
if [ -n "$git_root" ]; then
  agents_md="$git_root/AGENTS.md"
  claude_md="$git_root/CLAUDE.md"
  seek_present=0
  for f in "$agents_md" "$claude_md"; do
    [ -f "$f" ] && grep -q '/seek' "$f" 2>/dev/null && seek_present=1
  done
  if [ "$seek_present" = "0" ]; then
    target=""
    [ -f "$agents_md" ] && [ ! -L "$agents_md" ] && target="$agents_md"
    [ -z "$target" ] && [ -f "$claude_md" ] && [ ! -L "$claude_md" ] && target="$claude_md"
    if [ -n "$target" ]; then
      printf '\n- Uncertain about project term/schema/convention/prior decision → `/seek <topic>` first (lightweight KB lookup; same tier as grep/Glob).\n' >> "$target"
      echo "[seek] added /seek directive to $target" >&2
    fi
  fi
fi
