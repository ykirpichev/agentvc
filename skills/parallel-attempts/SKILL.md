---
name: parallel-attempts
description: Solve a coding task by running 2-4 genuinely different approaches in parallel, each in its own isolated workspace with the same acceptance check, then hand the results over for review instead of betting on the first idea. Use when the approach is uncertain, the user asks to try several approaches, best-of-N, a bake-off between models or agents, or a risky refactor or fix where a wrong first guess is expensive.
compatibility: Requires git. Uses the agentvc CLI when it is on PATH (shared builds, recorded history, review UI); otherwise falls back to git worktrees. Running attempts in parallel needs either subagents or a headless agent CLI (for example claude -p, codex exec, cursor-agent -p).
metadata:
  author: agentvc
  version: "0.1.0"
---

# Parallel attempts

One task, several isolated attempts, the same check for all, then a review. The
value comes from the attempts being **different**: three copies of the same idea
cost three times as much and teach nothing.

Never touch the user's checkout while attempts run. Never merge, promote or
delete anything without the user's go-ahead.

## 1. Decide whether to fan out

Fan out when at least one is true: there are two or more plausible designs; the
bug's cause is unclear; a refactor could go wrong in ways tests will show; the
user wants to compare models or agents. Do not fan out for a typo, a one-line
fix, or work whose approach the user already fixed. Say in one line why you
chose to fan out, and how many attempts (default 2, at most 4).

## 2. Write the brief and the check

Write one shared brief and one acceptance check before starting anything:

- **Goal and acceptance criteria**, in the user's words where possible.
- **Check command** that decides pass or fail with its exit code, for example
  `cargo test`, `npm test -- parser`, `pytest tests/test_x.py && ruff check .`.
  If the repo has no test for the goal, the first step of every attempt is to
  add one, and the brief says which file it goes in.
- **Constraints**: files not to touch, no new dependencies unless allowed,
  stay in your own workspace.

Then give each attempt a distinct **approach line** that commits it to a
different strategy. Good: "A: fix in the tokenizer; B: fix in the parser's error
recovery". Also good for model bake-offs: same approach line, different model.
Bad: "A: do it well; B: do it carefully". See
[references/briefs.md](references/briefs.md) for examples by task type.

## 3. Create isolated workspaces

Check once, with this single command: `agentvc status --json`.

- It succeeds: the repo has an agentvc space. Use agentvc, not worktrees.
- It fails with code `no_space`: agentvc is installed but the repo has no
  space. Ask the user before running `agentvc init`; use worktrees meanwhile.
- The command is not found: use git worktrees.

**With agentvc** (preferred: forks are near-instant and share build output,
history is kept, and the review UI can compare attempts).

```sh
agentvc task new "<goal>" --criteria "<criteria>" --json   # task.id N -> tN
agentvc run --detach --task tN --actor agent:<name> --prompt "<approach A>" \
  --check "<check command>" -- <agent command with the brief + approach A>
agentvc run --detach --task tN --actor agent:<name> --prompt "<approach B>" \
  --check "<check command>" -- <agent command with the brief + approach B>
agentvc ps            # running attempts; `agentvc log --task tN` when done
```

`--check` marks each attempt passed or failed. Details, including working in
the views yourself: [references/agentvc.md](references/agentvc.md).

**Without agentvc** (git worktrees). Commit or stash nothing of the user's;
start every attempt from `HEAD`:

```sh
base=$(git rev-parse HEAD)
git worktree add -b attempt/<slug>-a ../<repo>.attempts/a "$base"
git worktree add -b attempt/<slug>-b ../<repo>.attempts/b "$base"
```

Each worktree builds from scratch (no shared `target/` or `node_modules`), so
say so if builds are heavy, and suggest agentvc for next time. Details:
[references/git-worktrees.md](references/git-worktrees.md).

## 4. Run the attempts

Pick the first that fits:

1. **Subagents** (if your agent has them): one subagent per attempt, each told
   its workspace's absolute path, the brief, its approach line, and the check
   command; each must edit only under its path and finish by running the check
   and reporting the result and a three-line summary.
2. **Headless agent CLI**, one process per workspace, in the background, for
   example `claude -p "<brief>"`, `codex exec "<brief>"`, `cursor-agent -p "<brief>"`.
   With agentvc this is the command after `--` in `agentvc run`.
3. **Sequentially yourself**: work in workspace A, run the check, record it;
   then B. Slower, but still isolated and comparable.

If option 1 or 2 fails (a subagent is denied a tool, a CLI is missing), switch
to option 3 for that attempt instead of stopping, and say so in the results.

While they run, do not edit any workspace. Report progress only when an attempt
finishes or fails to start.

## 5. Collect results

For every attempt record: workspace or attempt id, approach, check result (exit
code), files changed and lines (`agentvc diff aN --stat` or
`git -C <ws> diff --stat $base`), and the agent's own summary. Commit or snapshot
each attempt's final state so nothing is lost:
`agentvc snapshot aN --meta tests=pass` or
`git -C <ws> add -A && git -C <ws> commit -m "attempt <x>: <approach>"`.

Show the user a table:

| Attempt | Approach | Check | Diff | Notes |
| --- | --- | --- | --- | --- |

If every attempt failed the check, say so first, with each failure's cause, and
suggest one changed brief rather than starting more attempts on your own.

## 6. Hand over for review

Recommend a review, not a winner by check result alone: passing tests do not
show scope creep, risk or maintainability. If the `attempt-review` skill is
installed, use it on these attempts. Otherwise give a one-paragraph comparison
and let the user choose.

Shipping is the user's call: with agentvc, `agentvc promote aN`, then
`agentvc rebase aN` and `agentvc pr aN`; with worktrees, merge or cherry-pick
the chosen `attempt/...` branch. Clean up only when asked: losers are evidence.
