#!/usr/bin/env bash
# memo.sh — context detection, persist, and dream-check for /memo
# Usage:
#   memo.sh detect
#   memo.sh persist <mode> <root> <project_name>
#   memo.sh dream-check <mode> <knowledge_path> <project_name>
set -euo pipefail

CMD="${1:-detect}"

_find_vault() {
  local vault_root=""
  for _c in ~/ws/dgv3 ~/ws-ps/dgv3 ~/dgv3; do
    [ -d "$_c" ] && { vault_root="$_c"; break; }
  done
  if [ -z "$vault_root" ]; then
    vault_root=$(rg --files --max-depth 4 ~ 2>/dev/null \
      | grep -m1 '/dgv3/' | sed 's|/dgv3/.*|/dgv3|') || true
  fi
  echo "$vault_root"
}

case "$CMD" in
  detect)
    # step 1: detect git context
    git_root=$(git rev-parse --show-toplevel 2>/dev/null || echo "")
    project_name=$(basename "$(git remote get-url origin 2>/dev/null \
      | sed 's/\.git$//')" 2>/dev/null || echo "")
    [ -z "$project_name" ] && project_name=$(basename "${git_root:-$(pwd)}")
    vault_root=$(_find_vault)

    # step 1: resolve mode — ir collection path is primary signal
    mode=""
    knowledge_path=""

    if command -v ir >/dev/null 2>&1 && [ -n "$project_name" ]; then
      ir_col_path=$(ir collection ls 2>/dev/null \
        | awk -v name="$project_name" '$1 == name {
            sub(/^[^[:space:]]+[[:space:]]+/, "");
            sub(/[[:space:]]+\[[^]]*\]$/, "");
            print; exit
          }')
      if [ -n "$ir_col_path" ]; then
        if [ -n "$git_root" ] && [[ "$ir_col_path" == "$git_root"/* ]]; then
          mode="project-mode"
          knowledge_path="$ir_col_path"
        elif [ -n "$vault_root" ] && [[ "$ir_col_path" == "$vault_root/notes-local/"* ]]; then
          mode="local-vault"
          knowledge_path="$ir_col_path"
        elif [ -n "$vault_root" ] && [[ "$ir_col_path" == "$vault_root/notes/"* ]]; then
          mode="vault"
          knowledge_path="$ir_col_path"
        fi
      fi
    fi

    # fallback: directory-based detection
    if [ -z "$mode" ]; then
      if [ -n "$git_root" ] && [ -d "$git_root/knowledge" ]; then
        mode="project-mode"
        knowledge_path="$git_root/knowledge"
      elif [ -n "$vault_root" ] && [ -d "$vault_root/notes-local/knowledges/$project_name" ]; then
        mode="local-vault"
        knowledge_path="$vault_root/notes-local/knowledges/$project_name"
      elif [ -n "$vault_root" ] && [ -d "$vault_root/notes/knowledges/$project_name" ]; then
        mode="vault"
        knowledge_path="$vault_root/notes/knowledges/$project_name"
      else
        mode="none"
        knowledge_path=""
      fi
    fi

    printf "mode=%s\ngit_root=%s\nproject_name=%s\nvault_root=%s\nknowledge_path=%s\n" \
      "$mode" "$git_root" "$project_name" "${vault_root:-}" "$knowledge_path"
    ;;

  persist)
    # step 6: index & persist
    mode="${2:?mode required}"
    root="${3:?root required}"
    project_name="${4:?project_name required}"

    ir update "$project_name" && ir embed "$project_name"

    if [ "$mode" = "project-mode" ]; then
      git -C "$root" add knowledge/
      # Scope the diff-check AND the commit to knowledge/ via pathspec. A bare
      # `git commit` writes the WHOLE staged index, so any pre-staged unrelated
      # work (e.g. in-flight code edits or `git mv` renames) would be swept into
      # the knowledge commit — splitting an atomic change and producing a
      # non-building snapshot. The pathspec commits only knowledge/ and leaves
      # everything else exactly as it was.
      git -C "$root" diff --cached --quiet -- knowledge/ \
        || git -C "$root" commit -m "knowledge: session $(basename "$root") - $(date +%Y-%m-%d)" -- knowledge/
    else
      vault_root=$(_find_vault)
      git -C "$vault_root" add \
        "notes/knowledges/$project_name/" \
        "notes-local/knowledges/$project_name/" 2>/dev/null || true
      git -C "$vault_root" diff --cached --quiet \
        || git -C "$vault_root" commit -m "knowledge: update $project_name - $(date +%Y-%m-%d)"
      git -C "$vault_root" push
    fi
    ;;

  dream-check)
    # step 7: determine if /dream should run
    # exits 0 = trigger dream (prints args to stdout, empty = full, "--sessions-only" = sessions-only)
    # exits 1 = skip
    mode="${2:?mode required}"
    knowledge_path="${3:?knowledge_path required}"
    project_name="${4:-}"

    if [ "$mode" = "project-mode" ]; then
      branch=$(git -C "$knowledge_path" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
      if [[ "$branch" == "main" || "$branch" == "master" ]]; then
        echo ""
        exit 0
      fi
      # feature branch: run --sessions-only when session files exist
      [ -z "$project_name" ] && project_name=$(basename "$(git -C "$knowledge_path" remote get-url origin 2>/dev/null | sed 's/\.git$//')" 2>/dev/null || basename "$(git -C "$knowledge_path" rev-parse --show-toplevel 2>/dev/null)")
      session_dir="$knowledge_path/sessions/$project_name"
      count=$(ls -1 "$session_dir"/session-*.md 2>/dev/null | wc -l | tr -d ' ')
      if [ "$count" -gt 0 ]; then
        echo "--sessions-only"
        exit 0
      fi
      exit 1
    else
      session_dir="$knowledge_path/session"
      [ ! -d "$session_dir" ] && exit 1
      count=$(ls -1 "$session_dir"/session-*.md 2>/dev/null | wc -l | tr -d ' ')
      [ "$count" -gt 3 ] && exit 0
      oldest=$(ls -t "$session_dir"/session-*.md 2>/dev/null | tail -1 || echo "")
      if [ -n "$oldest" ]; then
        mod_time=$(stat -f %m "$oldest" 2>/dev/null \
          || stat -c %Y "$oldest" 2>/dev/null || echo 0)
        now=$(date +%s)
        age_days=$(( (now - mod_time) / 86400 ))
        [ "$age_days" -gt 7 ] && exit 0
      fi
      exit 1
    fi
    ;;

  *)
    echo "Usage: memo.sh detect | persist <mode> <root> <project_name> | dream-check <mode> <knowledge_path>" >&2
    exit 2
    ;;
esac
