# Rubric

Score each dimension 0-3. Each score needs one line of evidence.

| Dimension | 0 | 1 | 2 | 3 |
| --- | --- | --- | --- | --- |
| **Correctness** (criteria met) | misses the goal | meets some criteria | meets all, one doubtful | meets all, each shown by a check or a read of the code |
| **Checks** | does not build | builds, required checks fail | required pass, optional (lint, type) fail | all pass |
| **Scope** | rewrites unrelated code | several unrequested changes | one small unrequested change | only what the goal needs |
| **Risk** | breaks an API, data or security without saying | risky change, untested | risky change, tested | no risky change, or risk isolated and tested |
| **Tests** | removes or weakens tests | no new tests for new behavior | tests the happy path | tests the behavior and its edge cases |
| **Maintainability** | hard to follow, duplicated logic | works but awkward | clear, minor nits | clear, matches the codebase's patterns |

## Weights

Default: Correctness x3, Checks x3, Risk x2, Tests x2, Scope x1, Maintainability x1
(max 36). Change the weights only if the user names priorities (for example a
spike where maintainability does not matter), and say so in the report.

## Gates

- Checks = 0 or 1 on a **required** check: "not shippable", ranked below every
  shippable candidate regardless of total.
- Risk = 0: "not shippable until fixed", even with a high total.
- Tests = 0 (tests removed or weakened): call it out in the verdict line.

## Ties

Within 2 points: prefer the smaller diff, then the one with better tests, then
the one whose approach the user said they preferred. Say it was close.
