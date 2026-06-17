---
slug: issue-pane-layout
kind: coding
title: TUI issue pane layout conventions
description: Layout conventions for keeping issue metadata compact while giving plan, review, steering, and agent logs the flexible space they need.
keywords: [Issue view, overview issue detail, fixed metadata header, Agent Log, plan checklist, review findings, phase detail, ratatui layout, TUI labels, compact metadata]
created: 2026-06-17
modified: 2026-06-17
---

# TUI issue pane layout

Issue-oriented views reserve fixed space for stable metadata, then give flexible
space to working details and logs.

| Surface | Layout rule |
|---------|-------------|
| Full Issue view | fixed 9-line metadata header; remaining area split between plan/review/steering and Agent Log |
| Overview issue detail | fixed-height metadata and phase-detail blocks; remaining vertical space goes to Agent Log |

Labels use operator-facing terms:

| Internal concept | Label |
|------------------|-------|
| subtasks | Plan checklist |
| findings | Review findings |
| latest log | `Agent log: <filename>` |

Keep the log renderer shared with `agent-log-prettifier.md` so overview and full
Issue view do not diverge.
