---
name: note
description: Creates or updates vault notes — raw captures, factual knowledge, or synthesis articles. Standalone, works inside or outside project context. Always searches before writing.
allowed-tools: Read, Glob, Grep, WebSearch, WebFetch, Bash
argument-hint: "[topic or concept]"
---

# note

Personal/learning knowledge substrate. Fully self-contained. Never calls back to `/memo`.

## Modes

**create** (default): full workflow Steps 1–7 from a topic or concept.

**update** `{path} domain:{topic_domain}`: caller already wrote the file. Skip Steps 1–4. Run Steps 5–7 (wikilinks, persist, wikify, journal) on the existing file at `{path}`.
- Infer note type from frontmatter tags: `source` → raw, `knowledge` → knowledge, else → article. Type governs wikilink rules (Step 5) and collection (`vault` vs domain) in Step 6.
- `{topic_domain}` = topic domain for wikify routing (e.g. `ai`, `rust`) — not the note's own ir collection (articles/raw use `vault`).

## Note Types

| Type | Trigger | Destination | ir collection |
|------|---------|-------------|---------------|
| **raw** | "capture", "save source", quick clip | `notes/sources/` | vault |
| **knowledge** | factual/definitional, reusable concept | `notes/knowledges/{domain}/` | domain |
| **article** | synthesis, interpretation, learning, opinion | `notes/` | vault |

If type is ambiguous: factual/definitional → knowledge; interpretation/opinion → article; unprocessed → raw.

---

## Step 1: Determine Canonical Title & Filename

Use the concept's own name directly. No prefixes. Filename = title + `.md`.

| Input | Filename |
|-------|----------|
| 삼태극의 이태극 | `이태극 (Dual Taeguk).md` |
| 천지인 개념 | `천지인 (Cheon-Ji-In).md` |
| binary black hole theory | `이원 블랙홀 (Binary Black Hole).md` |
| Triskelion (Celtic) | `트리스켈리온 (Triskelion).md` |

**Naming rules:**
- Korean concept → `한국어 이름 (English Name).md`
- English-only concept → `English Name.md`
- No compound prefixes: ~~`삼태극 — 이태극`~~, ~~`이원 블랙홀 — 퍼스`~~
- English translation goes in parentheses in filename AND in `aliases:`

---

## 2. Search First

Use Skill tool: `find-vault` → sets `$vault_root`

```bash
# exact + fuzzy match in vault
ls "$vault_root/notes" | grep -i "{keyword}"
ls "$vault_root/notes/knowledges" -R | grep -i "{keyword}"
```

Use Skill tool: `seek {canonical_title}, {Korean_variant}, {English_variant}`

- **Found** → read fully, update in place. Preferred outcome.
- **Not found** → create new.

---

## 3. Read Related Knowledge (knowledge and article types)

Use Skill tool: `seek {canonical_title}`

Read results ≥ 0.15 ir score. Link to them; do not copy content.

---

## 4. Write Content

For any diagram (flow, hierarchy, sequence, state): `Use Skill tool: mermaid-diagram {type} {description}`



## 4.0 Explanation Rules

When the note teaches a concept, method, or mechanism, do not rely on internal jargon alone.

Prefer this order:

1. 문제 / 맥락
2. 핵심 축이나 변수
3. 구성요소
4. 동작 순서
5. 왜 중요한가

### Define before naming

Do not open with compact technical terms if the reader may not know them.

Bad:

```text
We use piecewise lerp over leftChain/rightChain.
```

Better:

```text
먼저 길을 따라가는 좌표축을 만든다.
그 다음 외곽선을 왼쪽 경계와 오른쪽 경계로 다시 읽는다.
같은 위치에서 왼쪽-중앙-오른쪽 점을 잡고 두 구간으로 나눠 보간한다.
이 방식을 piecewise lerp라고 부른다.
```

### Jargon rule

- 새 용어는 등장 즉시 plain-language 설명을 붙인다
- 가능하면 한국어 설명 먼저, 기술 용어는 뒤에 괄호로 붙인다
- 용어 이름만 나열하지 않는다

Preferred:
- 길을 따라가는 좌표축(`s`)
- 끝단 캡(`cap`)
- 왼쪽 경계 체인(`leftChain`)

Avoid:
- `s`, `cap`, `chain`, `parameterization`만 던지고 설명하지 않기

### Breakdown depth

If a concept is hard, explain it as:

1. 한 문장 요약
2. 용어 2~3개 정의
3. 작은 예시
4. 단계별 설명

The note should teach the mechanism, not just preserve terminology.

### raw

Minimal frontmatter + verbatim source. No synthesis.

```yaml
---
title: "{Title}"
author: vlwkaos
created: YYYY-MM-DDTHH:mm:ss
access: private
tags: [source, {domain}]
source: "{URL}"
---
```

### knowledge

Frontmatter + factual body only. No opinion.

```yaml
---
title: "{Title}"
author: vlwkaos
created: YYYY-MM-DDTHH:mm:ss
modified: YYYY-MM-DDTHH:mm:ss
access: private
tags: [knowledge, {domain}]
aliases: ["{English title}", "{Korean title}"]
---
# {Title}
## Overview
## Content
```

Knowledge notes should optimize for explanation, not compression.

- Do not assume the reader already knows the jargon.
- If a method name appears, explain what it is and how it works.
- Prefer short subsections like:
  - `## Overview`
  - `## Core Concepts`
  - `## How It Works`
  - `## Why It Matters`

### article (synthesis)

```yaml
---
title: "{Canonical Title}"
author: vlwkaos
created: YYYY-MM-DDTHH:mm:ss
modified: YYYY-MM-DDTHH:mm:ss
access: private
tags: [note, {domain}]
aliases: ["{English title}"]
---
# {Canonical Title}
## Overview
{한 줄}
## Content
### 배경
### 전개
### 판단
{opinions/interpretation here only}
## See also
```

Prose: 현대 한국 소설 어투. 500–1500자. Split at 800+ words with multiple themes.
Technical terms: original language + Korean gloss on first occurrence.
Definitions belong in knowledge files — do not define inline.

For article type:
- keep readability above density
- if a term is crucial, briefly restate it in plain language even if a linked knowledge note exists
- never stack multiple unexplained method names in one paragraph

**Housekeeping on any file touched**: draft (no frontmatter / <3 sentences) → reformat; >0.7 overlap with another → merge; >1 distinct concept → split.

---

## 5. Wikilinks

Rules by type:

**knowledge** — links to other `knowledge` files only. Never link to articles.
- Use Skill tool: `seek {topic}` to find related knowledge files
- Add wikilinks on first occurrence: `[[knowledges/domain/filename|concept]]`
- Add reciprocal backlink in each linked file's `## See also`

**article** — three link categories; use `seek` with varied query angles:
- **Related**: same or overlapping topic → `[[knowledges/...]]` or `[[note-title]]`
- **Contrasting**: opposing angle or alternative framing → `[[...]]` with note in `## See also`
- **Interesting-vector**: semantically adjacent but unexpected → `[[...]]`
- Also cite `raw` sources directly referenced during writing
- Add reciprocal backlink in all linked files' `## See also`

**raw** — no outgoing wikilinks (terminal node; others reference it).

---

## 5.5 INDEX.md + RESOURCE.md (knowledge type only)

**INDEX.md** at `notes/knowledges/{domain}/INDEX.md`:
- Missing → create:
  ```markdown
  # {Domain} Knowledge Index
  | File | Description |
  |------|-------------|
  | [[filename\|Title]] | one-line description |
  ```
  List all existing files in the domain + the new file.
- Exists → add/update entry for the newly written file.

**RESOURCE.md** at `$vault_root/notes/RESOURCE.md`, Domains table:
- New domain → add row: `| [[knowledges/{domain}/INDEX\|{domain}]] | key content summary |`
- Existing domain with old representative-file link → update link to `[[knowledges/{domain}/INDEX\|{domain}]]`
- Existing domain already pointing to INDEX → update key content summary if needed.

---

## 6. Save, Index & Persist

Write file to resolved path. Then:

```
collection: raw/article → "vault"  |  knowledge → domain name (e.g. "rust", "philosophy")
```

Run: `bash ~/.claude/skills/note/scripts/note-persist.sh "$vault_root" "{collection}" "{canonical_title}"`

Requires `dangerouslyDisableSandbox: true`. If script is missing: `ls ~/.claude/skills/note/scripts/` to verify.

After persist: `Use Skill tool: wikify {saved_note_path} {topic_domain}`

`{topic_domain}` = for knowledge, same as `{collection}` (the domain name). For article/raw, infer from frontmatter tags — first non-generic tag after stripping `note`, `source`, `learning`, `knowledge` (e.g. tags `[note, ai]` → `ai`). Default: `vault`.

---

## 7. Journal

Append to `$vault_root/notes/Journal.md` under `## Journaling`:

```markdown
- YYYY-MM-DD
	- [[{Canonical Title}]] — {한 줄 요약} ({updated|created})
```

Most recent date at top. Append under existing today entry if present.

---

## Invariants

- Search before writing. Always.
- Update existing > create new.
- Opinions only in `### 판단`.
- Do not create knowledge files from article flow — classify first.
- Obfuscate project/company names in `notes/` (concepts and patterns only).
- Multilingual: dual-query Korean + English; aliases include both; tags and slugs always English.
- Explanations should be understandable without prior jargon familiarity.
