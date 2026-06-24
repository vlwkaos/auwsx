---
slug: no-repeat-breadcrumbs
kind: coding
title: no-repeat breadcrumbs in auwsx verification policy
description: When to use tests, good-to-go axes, no-repeat breadcrumbs, or memo knowledge after failures and design decisions.
keywords: [no-repeat breadcrumb, good-to-go axes, executable test, regression test, boundary test, memo knowledge, session knowledge, verification policy, machine-verifiable checks, inline caret comment, failure prevention]
created: 2026-06-24
modified: 2026-06-24
---

# no-repeat breadcrumbs

Promotion order for recurring correctness checks:

| Tool | Use when |
|------|----------|
| executable test | concrete bug/regression at a boundary |
| good-to-go axis | recurring review invariant or checklist that spans surfaces |
| no-repeat breadcrumb | smallest local hint after a specific mistake or wrong assumption |
| memo/session knowledge | multi-faceted design decision or branch continuation |

No-repeat is not the primary machine-verification mechanism. Prefer tests for
checkable behavior and good-to-go axes for review invariants. Use a breadcrumb
when a future reader needs a local warning at the exact offending line or habit;
prefer an inline `^` comment at that code path over expanding `AGENTS.md`.
