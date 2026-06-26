---
name: write-test-audit
description: Audit existing test files — removes trivial/useless tests, deduplicates, then extends coverage via isolated subagents. Use when a test file feels shallow, was written in-context, or before any test-gated release.
argument-hint: "[test-file-or-glob]"
allowed-tools: Bash, Read, Glob, Grep, Agent
---

# Write Test Audit

## Trigger

Use when:
- Test file was written in the same session as the implementation
- Coverage feels shallow or tests seem to restate function names
- Pre-release quality gate on test suite
- After merging a feature branch

## Workflow

### 1. Collect Tests

Read every test in scope. For each test record:
- Test name
- Assertions it makes
- What input class it covers (happy, null, boundary, error, concurrent)

### 2. Spawn Purge Agent (Agent — general-purpose)

Pass the full test list. The agent has NO access to source code.

```
You are reviewing a test file without seeing the source code.

Flag tests as USELESS if they meet ANY of these:
- Assertion restates the function/method name (e.g., test "add returns sum" asserts add(1,2) === 3 with no other boundary)
- No assertion at all
- Asserts only that the function does not throw (when no error contract is documented)
- Duplicate of another test (same input class, same assertion shape, different variable names only)
- Trivially true (e.g., asserts a constant equals itself)

For each flagged test: output REMOVE <test name> — <reason>
For each kept test: output KEEP <test name>

[paste test list]
```

Apply removals. Report count removed.

### 3. Spawn Gap Finder (Agent — general-purpose)

Pass the KEPT tests + the public contract (signatures only, no implementation):
```
Given only this test list and public contract, identify coverage gaps.
You have NOT seen the implementation.

Find:
- Input classes with no test (null, empty string, max int, wrong type, negative, concurrent)
- Error paths with no test
- State transitions not covered
- Any input that is structurally valid but semantically wrong

Output a list of missing test cases as: MISSING: <given X, when Y, then Z>

[paste kept tests + signatures]
```

### 4. Spawn Test Writer for Gaps (Agent — general-purpose)

Pass ONLY the gap list and public contract. No existing tests, no implementation.

```
Write tests to fill these specific gaps. You have no knowledge of the implementation.

Rules:
- One assertion per test
- Name as "given X, when Y, then Z"
- Failing test = potential bug in source, NOT a test error
- No illustrative tests — must be runnable

[paste gap list + signatures]
```

### 5. Integrate and Run

1. Remove flagged tests from the file.
2. Append new gap-filling tests.
3. Run the test suite.
4. Failing tests from NEW tests: surface to user as potential bugs, do not auto-fix.
5. Failing tests from EXISTING (kept) tests: investigate — likely a regression.

## Output Report

```
Audit complete: <file>
  Removed: N tests — <brief reasons>
  Added:   M tests — <gap classes covered>
  Failures: <list or "none">
```

## Hard Rules

- Purge and gap-finding are always done by agents with NO source code access
- A removed test is not a loss — if it was useless, coverage did not decrease
- Never auto-fix a newly failing test to match the source — it is a finding
