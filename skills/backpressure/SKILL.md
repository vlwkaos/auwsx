---
name: backpressure
description: "Pre-implementation requirement clarification + post-implementation quality loop (good-to-go, write-test, write-test-audit, simplify, sec). Run before implementing anything unclear, or after any change to iterate until quality and security are met. Invoke when asked to /backpressure."
allowed-tools: Bash, Read, Agent
argument-hint: "[pre|post|full]"
---

# Backpressure

Two-phase skill. `pre` = requirement clarification. `post` = quality loop. `full` (default) = both.

---

## Phase 1 — Requirement Clarification (pre / full)

**Goal:** Surface all ambiguity and logical caveats before a single line is written.

### 1. State Expected Behavior

Write a concrete, numbered list of expected behaviors for the task:
- What happens in the happy path
- What happens in each edge/fallback case
- What explicitly does NOT change

No hedging. If uncertain about any item, flag it inline with `[?]`.

### 2. Identify Caveats in Your Own Logic

Re-read the list. Ask:
- Does any step contradict another?
- Does any step assume something not confirmed?
- Is there a case where two behaviors overlap or conflict?
- Are there callsites or consumers not covered by the stated behavior?

Then apply a security lens to the same list:
- Does any step handle untrusted input (network, user, file, env) without validation at the boundary?
- Does the task touch secrets, credentials, auth, or permissions?
- Does it add a dependency, shell out, or call an external service?
- Could any step widen an attack surface (new endpoint, deserialization, dynamic exec, path/SQL construction)?

Add any found issues to the list as `[CAVEAT]` items. Tag security ones `[CAVEAT:sec]`.

### 3. Present to User

Output the full behavior list. For each `[?]` or `[CAVEAT]`, ask a direct question.

**Do not proceed to implementation until all `[?]` and `[CAVEAT]` items are resolved by the user.**

### 4. Reiterate

After user responds, rewrite the behavior list with resolved answers incorporated. Repeat from step 2 until the list is clean. One clean reiteration with no open items = done.

---

## Phase 2 — Quality Loop (post / full)

Run after implementation. Repeat until all checks pass. Each round spawns a separate agent to protect main context.

### Round structure

```
1. /good-to-go <scope>          — docs, build, consistency, test coverage audit
2. /write-test                  — write tests for any new/changed code without tests
3. /write-test-audit            — audit existing test files touched by the change
4. /simplify                    — simplify recently changed code
5. /sec                         — security audit: secrets, supply chain, attack surface
```

Run `/sec` when the change touches any of: dependencies, secrets/credentials,
auth/permissions, untrusted input, network calls, shelling out, or file/path
handling. Skip only for pure-internal refactors with none of the above — and say
so explicitly when skipping.

If any step produces FAILs or new issues:
- Fix them in the main context
- Spawn a new round (fresh agent)
- Repeat until a full round produces no FAILs

### Scope mapping

| ARGUMENTS | good-to-go scope |
|-----------|-----------------|
| (empty) / full | commit |
| post commit | commit |
| post branch | branch |
| post all | all |

### Termination

A round is clean when:
- `/good-to-go` reports no FAIL items
- `/write-test` finds no untested paths
- `/write-test-audit` finds no useless/missing tests
- `/simplify` finds no changes to make
- `/sec` reports no findings (or was justifiably skipped)

Report: how many rounds ran, what each round fixed.
