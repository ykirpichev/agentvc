# Review: <task goal>

**Verdict:** <one line: which to ship and the main reason, or "none is shippable">

Criteria: <list> · Checks: `<cmd>`, `<cmd>` · Base: `<rev>`

| Rank | Candidate | Approach | Checks | Diff | Score /36 | Shippable |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | a5 | <one line> | 3/3 pass | +84 -12, 4 files | 31 | yes |
| 2 | a3 | <one line> | 2/3 (lint) | +210 -40, 9 files | 24 | yes |
| 3 | a4 | <one line> | tests fail | +60 -5, 2 files | 12 | no |

## a5 (recommended)

- Strengths: <with evidence>
- Problems: <file:line, what>
- Scores: Correctness 3 (<evidence>), Checks 3, Scope 3, Risk 2 (<evidence>), Tests 2, Maintainability 3

## a3

...

## Graft list

- From a3: the edge-case test in `tests/parser.rs:120-150`.
- From a4: the clearer error message in `src/lex.rs:88`.

## Next steps (not run)

```sh
agentvc promote a5
agentvc abandon a4 --reason "tests fail: <one line>"
agentvc fork --from a5        # apply the graft list here, if you want it
agentvc rebase a5 && agentvc pr a5
```
