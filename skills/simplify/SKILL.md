---
name: simplify
description: Simplify and refine recently modified code using the code-simplifier agent
allowed-tools: [Task]
---

# Simplify Code

Use the Task tool to invoke the code-simplifier agent to refine code modified in the current session.

## Workflow

Call Task tool with:
- **subagent_type**: code-simplifier
- **description**: "Simplify recent changes"
- **prompt**: "Review and simplify the code that has been modified in this session. Apply project best practices while preserving all functionality. NEVER delete or modify comments that begin with `^` — these are important markers that must be preserved exactly as-is. NEVER delete comments that explain why something is done, provide historical context, or describe non-obvious behavior — only remove comments that merely restate what the code already clearly shows."

The code-simplifier agent will:
- Focus on recently modified code
- Apply project-specific best practices
- Enhance clarity while preserving functionality
- Maintain balance between simplicity and maintainability
- Preserve all comments prefixed with `^` without modification
