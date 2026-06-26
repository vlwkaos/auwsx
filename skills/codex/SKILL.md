---
name: codex
description: Use when running OpenAI Codex CLI (`codex exec`, `codex resume`) for code analysis, refactoring, or edits.
---

## Prepare Prompt (main agent)

User instructions are often vague ("fix per analysis"). Codex cannot see this conversation — compose a self-contained prompt with:
- **What** — files, functions, changes
- **Why** — the motivating finding
- **Constraints** — preserve API, don't touch tests, etc.

Then ask once via `AskUserQuestion`:
- **Model** — `gpt-5.4` (recommended), `gpt-5.3`, `o3`; Other accepts any `-m` value. `*-codex` variants (`gpt-5.4-codex`, `gpt-5.3-codex`) are NOT available on ChatGPT-account plans — use the plain model name.
- **Reasoning effort** — `xhigh` | `high` | `medium` | `low`

Run `codex exec` with the composed prompt, not the raw user instruction.

## Command

Always include `--skip-git-repo-check` and append `2>/dev/null` (suppresses thinking tokens; omit only if user asks for them). Always pass `-m <MODEL> --config model_reasoning_effort="<level>"`.

| Use case | Flags |
|---|---|
| Read-only review (default) | `--sandbox read-only` |
| Apply local edits | `--sandbox workspace-write --full-auto` |
| Network/broad access | `--sandbox danger-full-access --full-auto` |
| Resume session | `echo "prompt" \| codex exec --skip-git-repo-check resume --last 2>/dev/null` |
| Run from other dir | add `-C <DIR>` |

Ask before using `--full-auto` or `danger-full-access` if not pre-authorized.

## Return to Main Agent

```bash
git diff --stat
git diff --name-only
```

```
## Codex Result
**Status**: success | partial | failed
**Changed files**: path (added|modified|deleted)
**Summary**: 1-3 sentences
**Output**: [relevant stdout]
```

Then use `AskUserQuestion` for next step (continue, resume, stop).

## Critical Evaluation

Codex runs OpenAI models with their own cutoffs — treat as **colleague, not authority**.
- Trust your own knowledge; push back when confident.
- WebSearch disagreements before accepting Codex's claims.
- When disagreeing, identify yourself:
  ```bash
  echo "This is Claude (<model>). I disagree with [X] because [evidence]." | codex exec --skip-git-repo-check resume --last 2>/dev/null
  ```

## Errors

- Non-zero exit on `codex --version` or `codex exec` → stop and report.
- Warnings/partial → summarize, ask how to adjust.
