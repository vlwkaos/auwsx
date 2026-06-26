---
name: seek
description: |
  Search project + domain knowledge base via ir (semantic) + filename rg fallback. Returns ranked paths + 1-line excerpts; never reads files. Same ergonomic tier as grep/Glob.
  TRIGGER when: about to grep for a concept (not a literal string); about to answer a question about project domain/schema/convention/architecture/API; about to assume how the project works; user names a project resource (API, URL, doc, slug, term, acronym); error hit with potentially-known cause.
  SKIP: literal-string grep across code; pure syntax/language question; /recall already invoked this turn; user provided the answer inline.
allowed-tools: Bash, Read
argument-hint: "<topic or keywords> [extra-collection ...]"
---

# seek — tier-2 knowledge lookup

## When to invoke (reflex triggers)

| Situation | Action |
|-----------|--------|
| About to guess at a project term, schema, convention, API | `/seek <term>` first |
| About to grep for a concept (not a literal string) | `/seek <concept>` first |
| Hit an error / unclear behavior — might be known | `/seek <error keywords>` |
| User asks about a project resource | `/seek <resource>` |
| Agent is unsure what to do and local/project knowledge may resolve it | `/seek <keywords>` before guessing or asking |
| Need full planning context (architecture, active tasks) | use `/recall` instead, not /seek |

## Deterministic retrieval rule

When operating under the unified KB flow, `/seek` is the minimal-footprint reflex for uncertainty. It should fire before relying on generic model memory for project/domain/provider/methodology claims, but it should remain lightweight: show ranked hits first, then read only the top 1-2 directly relevant files.

## Run

```bash
bash ~/.claude/skills/seek/scripts/seek.sh "$ARGUMENTS"
```

Runs in the default sandbox (ir search is local-read; the script writes to `$TMPDIR` only).

`$ARGUMENTS` = topic, error keywords, or comma-joined terms. Append extra ir collections positionally if needed: `/seek "lifecycle hook" rust`.

## Output

Numbered list. Each row: `N. <path>  (<source> <score>)` + indented excerpt line.

Sources: `ir:<collection>` (semantic hit), `filename` (project knowledge dir), `vault-filename` (vault knowledges).

## What to do with results

1. Skim the numbered list — do NOT read everything.
2. Read 1-2 top hits with the Read tool, only if they look directly relevant.
3. If nothing matches, fall back to grep/Explore (you tried KB first — that was the point).

## Do NOT

- Do NOT spawn a subagent for this — it's tier-with-grep, must stay inline.
- Do NOT auto-read every hit — display first, read selectively.
- Do NOT use /seek when the user already invoked /recall — recall already searches broader.
