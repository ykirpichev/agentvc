---
name: stack-workflow
description: How to do version control in a repository managed by agentvc - one branch per change on the workspace's stack, review fixes into the right branch, PRs through stack_submit, and review comments answered in their threads. Use whenever the repository uses agentvc, or the user mentions a stack, stacked PRs or agentvc.
---

# Working on an agentvc stack

agentvc keeps this workspace's **stack**: branches on top of each other, one
pull request per branch, each based on the branch below it. Every checkout
(each editor window) has its own stack. Do version control through the
agentvc MCP tools (or the `agentvc stack` CLI with `--json`), not raw git:
branches made with `git checkout -b` or commits made with `git commit` don't
join the stack.

## Know where you are

- `stack_log` shows the stack bottom to top, which branch is checked out
  here, each branch's PR and checks, and what needs a restack.

## One branch per change

- When a change is done, `stack_create` with a commit message (and a short
  branch name if you like) commits everything you changed as a new branch on
  top of the current one. Keep going on top of it for the next change.
- Asked for several PRs, or for "a stack"? Make one branch per PR this way, in
  the order they should merge.
- `insert: true` puts the new branch between the current one and the branches
  above it.

## Fixes go into their branch

- `stack_modify` puts what you changed into the current branch (amends its
  last commit, or adds one with `new_commit: true`) and restacks the branches
  above it.
- After a branch below moved, `stack_restack`. After PRs merged, `stack_sync`
  pulls the trunk, drops merged branches and restacks the rest. On a conflict
  nothing changes and the error lists the paths: tell the user.

## Pull requests

- `stack_submit` pushes the stack and opens or updates one PR per branch. Only
  submit when the user asks you to.

## Review comments

- Before you end a turn or say you are done, call `review_comments`: it lists
  the open threads on what you work on, with file and line. Fix each one,
  `reply_comment` to say what you changed, and `resolve_comment` it (that marks
  it addressed; only a person closes it).
- If a comment is unclear, `reply_comment` with your question instead of
  guessing. Threads marked outdated were written on lines that have since
  changed: check they still apply before acting on them.

## Attempts

The attempt tree (forks, snapshots, compare, promote) stays available through
the slash commands `/attempt`, `/attempts`, `/compare` and `/promote`. Its MCP
tools are off unless the server runs as `agentvc mcp --attempts`.
