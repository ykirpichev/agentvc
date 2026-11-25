# Writing briefs that produce different attempts

Every attempt gets the same shared brief plus one approach line. The shared brief:

```
Goal: <one sentence>
Acceptance: <criteria the user gave, as a list>
Check: <command>; it must exit 0 when you are done.
Workspace: <absolute path>. Read and edit only inside it. Do not touch git config,
other workspaces or the user's checkout.
Constraints: <no new dependencies / keep the public API / files not to touch>
Approach: <the attempt's own line>
When done: run the check, then reply with PASS or FAIL, the files you changed,
and three lines on what you did and what you are unsure about.
```

## Approach lines by task type

**Bug with an unclear cause**
- A: reproduce with a failing test first, then fix at the lowest layer that explains the failure.
- B: assume the bug is in the caller's handling of edge cases; fix there and add the edge-case tests.

**New feature with design choices**
- A: smallest change that satisfies the criteria inside existing modules.
- B: introduce the abstraction you would want if three more similar features followed.

**Refactor**
- A: mechanical, behavior-preserving steps, tests green after each step.
- B: rewrite the module behind the same interface, then delete the old one.

**Performance**
- A: algorithmic change (data structure or complexity).
- B: caching or batching without changing the algorithm.
Add a benchmark command to the check, and ask each attempt to report numbers.

**Dependency upgrade**
- A: upgrade and fix call sites directly.
- B: add a thin adapter so call sites do not change.

**Model or agent bake-off**
Same approach line for every attempt; vary only the model or agent, and record
which one ran in each attempt (with agentvc: `--actor agent:<name> --model <m>`).

## Signs the attempts are not different enough

- The approach lines would produce the same first edit.
- One line is a strict subset of another ("fix it" vs "fix it and add tests").
- You cannot say in one sentence what you would learn if B beat A.

Rewrite the lines before starting.
