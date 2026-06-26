---
name: write-test
description: Write tests from an isolated third-person context — spawns a blind subagent to write tests, a second for adversarial review, and optionally Codex for an independent angle. Never writes tests inline in the current context. Use whenever tests need to be written for any code.
argument-hint: "[file-or-description]"
allowed-tools: Bash, Read, Glob, Grep, Agent
---

# Write Test

Tests written in the same context as implementation share its assumptions. This skill breaks that by delegating to isolated subagents that have no knowledge of the current task's intent.

## When to Use

- Adding tests for existing code
- Writing tests before/alongside a feature
- Verifying a fix actually works
- Peer-reviewing test quality

**Never write tests directly in the current conversation.** Always invoke this skill.

## Workflow

### 1. Extract Minimal Context

Gather only what a stranger would need to write fair tests — no implementation rationale, no intent commentary:

```
- Target: <file path(s) and function/class names to test>
- Public contract: <function signatures, return types, thrown errors>
- External behaviors: <side effects, I/O, state mutations>
- Edge inputs: <null, empty, overflow, wrong type, concurrent>
- Existing test file (if any): <path>
- Test framework: <jest|pytest|vitest|mocha|etc>
```

### 2. Spawn Blind Test Writer (Agent — general-purpose)

Prompt must NOT include:
- Why the code was written
- What bug it fixes
- What the implementation does internally

Prompt template:
```
Write tests for the following code. You have no knowledge of the author's intent.
Your goal: assert the PUBLIC CONTRACT only. Test failure cases aggressively.

[paste minimal context from step 1]

Rules:
- One assertion per test
- Name tests as "given X, when Y, then Z"
- Cover: happy path, null/empty input, out-of-range input, wrong type, boundary values
- Do NOT mirror the implementation — test behavior, not code structure
- Do NOT add tests that trivially restate the function name
```

Wait for the agent to return tests. Save output to a scratch path in the project (e.g. `tmp/tests-draft.txt`).

### 3. Spawn Adversarial Reviewer (Agent — general-purpose)

Pass ONLY the generated tests (not the source code) to a second agent:
```
Review these tests for uselessness and gaps. You have NOT seen the source code.

Criteria:
- REMOVE: tests that assert only what the function name already says
- REMOVE: tests with no assertion or trivially true assertions
- REMOVE: tests that duplicate each other with only renamed variables
- ADD: any missing boundary, error, or concurrent-access cases you notice from the contract
- ADD: at least one test that inputs structurally valid but semantically wrong data
- REPORT: list of removed (with reason) and added tests

[paste generated tests]
```

### 4. Optional — Codex Independent View

If the project has a `codex` setup or the user asks for a third angle, spawn via the `codex` skill:
```
/codex exec "Write unit tests for <target>. Start from zero knowledge of implementation. Cover the public contract, failure modes, and boundary inputs."
```

Merge any non-duplicate tests from Codex output into the final set.

### 5. Integrate and Run

1. Write the merged, reviewed tests to the actual test file.
2. Run the test suite.
3. If any test fails unexpectedly: do NOT fix the test to match the code — surface the failure to the user as a potential bug.

## Output

Report:
- Number of tests written
- Tests removed by adversarial review (with reason)
- Any test failures (with the assertion that failed)
- Whether Codex was used

## Hard Rules

- Never write tests in the same context as the implementation being tested
- A test that asserts `result === functionName()` is useless — remove it
- Failure is signal — a failing test means the code may be wrong, not the test
- Tests must be runnable, not illustrative
