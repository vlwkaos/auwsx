---
name: memory-save
description: Save durable project memory through the configured auwsx Memory provider.
---

# Memory Save

Use this when an auwsx worker has a useful durable result, decision, command
correction, failure lesson, or shipped-change summary.

Preferred run shape:

```bash
"$AUWSX_BIN" memory save "$AUWSX_PROJECT_ID" --kind result --file <path>
```

For short notes:

```bash
"$AUWSX_BIN" memory save "$AUWSX_PROJECT_ID" --kind note "<text>"
```

Rules:

- Save only non-secret, durable knowledge.
- Do not edit project source files as part of this skill.
- The selected Memory preset decides the backend. `portable-markdown` appends
  local auwsx memory; `auwsx-skills` writes a session artifact that `/dream`
  can promote.
- Do not call provider-specific tools such as memo or note directly from phase
  prompts. This skill is the stable provider boundary.
- If setup is missing, report the memory command error clearly.
