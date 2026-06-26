# Implementation Doc Example

Example of a `coding/impl-*.md` file written when a feature is completed with decision rationale.

---

**File:** `coding/impl-mcp-server.md`

```markdown
---
name: impl-mcp-server
description: MCP server integration — architecture, protocol choices, alternatives rejected
keywords: mcp, model-context-protocol, server, tool, stdio, http, integration
---

## What It Does

Exposes ir's semantic search as MCP tools so Claude Code can call `ir search` and `ir collection ls`
without spawning a subprocess per query. Runs as a persistent stdio process managed by Claude Desktop.

## Architecture

```
claude-desktop ──stdio──► ir-mcp (bin/ir-mcp) ──► ir core (lib)
                              │
                         MCP protocol (JSON-RPC 2.0)
```

Entry point: `bin/ir-mcp` — reads JSON-RPC from stdin, dispatches to handlers, writes to stdout.

## Key Decisions

**stdio over HTTP**
- Chose stdio: Claude Desktop manages lifecycle, no port conflicts, no auth needed
- Rejected HTTP: adds networking complexity, requires daemon management, overkill for local use

**Thin handler layer**
- Handlers call `ir::search()` and `ir::collections()` directly (same lib as CLI)
- Rejected: spawning `ir` subprocess — redundant serialization, slower, no shared cache

**No streaming**
- MCP tools return complete results; search results fit in one response
- Deferred: streaming for large collection listings (>1000 docs)

## Protocol

Tool definitions (returned from `tools/list`):
- `ir_search` — params: `{ query: string, collection?: string, limit?: number }`
- `ir_collections` — params: `{}`

Error codes follow MCP spec: `-32602` invalid params, `-32603` internal (ir error).

## Alternatives Rejected

| Option | Reason rejected |
|--------|----------------|
| REST API wrapper | Claude Desktop doesn't support HTTP MCP servers yet |
| Separate process with socket | Complex IPC, no benefit over stdio |
| Embed in Claude Code plugin | Out of scope, requires different distribution |

## What Can Break

- Metal sandbox: `ir search` inside MCP server inherits Claude Desktop's sandbox — test with GPU access
- Collection path: if `~/knowledge` moves, MCP server needs restart (reads paths at startup)
```

---

## When to Create impl-*.md

Create when a session contains:
1. A feature built end-to-end (not just a fix or tweak)
2. Non-obvious choices made (algorithm, protocol, library, architecture pattern)
3. Alternatives explicitly rejected with reasoning
4. Integration points other code must know about

Do NOT create for:
- Bug fixes (session file is enough)
- Config changes
- Refactors that don't change observable behavior
