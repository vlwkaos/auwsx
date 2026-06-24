---
slug: prompt-policy
kind: coding
title: auwsx prompt policy and profile composition
description: How auwsx should persist bounded operator guidance without turning AGENTS.md into duplicate prompt authority.
keywords: [prompt policy, prompt profile, Pipeline UX Standard, operator guidance, immutable worker contract, prompt snippets, global settings, project override, prompt render order, prompt injection boundary, capability footer, bounded guidance]
created: 2026-06-24
modified: 2026-06-24
---

# auwsx prompt policy

Do not put pipeline/process expectations into global `AGENTS.md`: it spends
context everywhere and creates duplicate authority. Keep auwsx-owned behavior in
prompt rendering and make user-editable guidance structured, bounded, and
explicitly non-secret.

## Model

| Layer | Rule |
|-------|------|
| immutable worker contract | built into auwsx prompt rendering |
| prompt profile | selected globally or per project |
| snippets | compact packaged guidance: backpressure, good-to-go, no-repeat, human-verify, conflict handling, merge verification |
| project runtime knobs | project-local schedule, concurrency, merge policy, timeouts, skill path |
| editable guidance | bounded, labelled non-secret, delimited, cannot override system/developer/repo controls |

Prompt render order:
1. auwsx immutable worker contract
2. selected auwsx prompt profile snippets
3. repo instructions
4. issue state, subtasks, findings, queue, reports
5. exact issue-scoped control protocol

## Safety requirements

- `pipeline_ux_guidance` needs a max length, rows-affected checks, and tests.
- Delimited operator-guidance blocks must state they cannot bypass controls,
  reveal secrets, or override system/developer/repo rules.
- CLI output for persisted guidance must strip or escape ASCII control chars.
- Issue-local socket/proxy access must share the issue-scoped allowlist or the
  worker env must omit `AUWSX_SOCK`; `UpdateGlobalSettings` must not pass
  issue-local control.

## Verification

Add targeted tests for global settings migration/IPC roundtrip, dispatch, prompt
row editing, select/completion behavior, TUI capability rendering, and proxy
deny behavior. Broad tests passing is insufficient for this surface.
