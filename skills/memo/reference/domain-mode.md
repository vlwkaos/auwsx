# Domain Mode

Technology, language, or topic collections (rust, react, biology…). Flat structure — no subdirs.

## When to Use

- User explicitly names a domain (`in biology`, `in rust`, `in typescript`)
- Collection name is a technology, language, or topic — not a product/app name
- Collection name differs from current git project name

## Setup (first time only)

```bash
mkdir -p "$knowledge_path"
ir collection add "{collection_name}" "$knowledge_path"
# Create INDEX.md if missing (see Update INDEX.md below)
```

## File Locations

All files flat in `{knowledge_path}/`:

| File | Content |
|------|---------|
| `{topic}.md` | Concepts, patterns, reference for the domain |
| `session-{timestamp}.md` | Session file (flat, no session/ subdir) |
| `INDEX.md` | Table of all files in the collection |

No `bigpicture/`, `coding/`, `domain/`, or `session/` subdirectories.

## Update INDEX.md

Maintain `{knowledge_path}/INDEX.md`:
```markdown
# {Collection} Knowledge Index
| File | Description |
|------|-------------|
| {filename}.md | {one-line description} |
```

Add/update entry for each file written this session.

## Report Format

```
Saved:
- {topic}.md (new|updated — {one-line})
- session-{date}.md (new)
- INDEX.md (updated)
```
