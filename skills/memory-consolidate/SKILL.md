---
name: memory-consolidate
description: Consolidate or audit project memory through the configured auwsx Memory provider.
---

# Memory Consolidate

Use this for routine maintenance. This is a memory operation, not permission to
edit project source files.

Dream consolidates session memory into durable knowledge:

```bash
"$AUWSX_BIN" memory consolidate "$AUWSX_PROJECT_ID" --mode dream
```

Deepsleep audits and housekeeps durable knowledge:

```bash
"$AUWSX_BIN" memory consolidate "$AUWSX_PROJECT_ID" --mode deepsleep
```

Rules:

- Stay within the configured Memory provider.
- Treat `dream` and `deepsleep` as distinct interfaces; do not substitute one
  for the other.
- Do not bypass the issue pipeline for source changes.
- Report setup problems clearly, including missing provider scripts or tools.
- Keep the result concise enough for the routine log and main-job history.
