---
name: attempt-workflow
description: How to work inside an agentvc space - fork an attempt before a risky or alternative approach, record outcomes, and never edit outside your own view. Use whenever the repository has a .agentvc directory or the user mentions agentvc, attempts, or the attempt tree.
---

# Working in an agentvc space

agentvc records every try at a task as an **attempt**: a node in a tree, with
its own copy-on-write directory (its **view**). Hooks snapshot your view after
each edit and at the end of each turn, so the history is kept for you. Your
job is to keep the tree honest.

Ids: tasks are `t1`, attempts `a3`, snapshots `s7`.

## Know where you are

- `agentvc log` (or the MCP `log` tool) shows the tree. `agentvc show aN` shows
  one attempt and its view path.
- The directory you work in decides your attempt. The main checkout is the root
  attempt `a1`; views live under `.agentvc/views/aN`.

## Fork before a risky or alternative approach

Fork when you are about to try something you may want to throw away: a large
refactor, a second design, a dependency upgrade, a speculative fix.

- The user can run `/attempt [what you will try]`. You can call the MCP `fork`
  tool (or `agentvc fork --actor agent:claude-code --prompt "..."`).
- A fork stays on its parent's task unless you pass another `task`.
- After forking, `cd` into the new `view_path` and use absolute paths inside it
  for every read and edit.

To try two approaches, fork twice from the same parent, not one from the other.

## Never edit outside your view

- Do not edit the main checkout, `.agentvc/`, or another attempt's view.
- If you notice you have been editing the wrong place, stop and tell the user.

## Record outcomes

- At milestones, call `snapshot` with meta, for example `{"tests": "pass"}` or
  `{"note": "parser rewritten, benchmarks pending"}`.
- When you know how an attempt went, `mark` it `passed` or `failed` with a
  short reason. Marking does not close it.
- Leave `promote`, `abandon` and `pr` to the human unless they ask you.

## Show the human

The review UI (`agentvcd`, default http://localhost:7420) shows the tree:
`/attempts/N` for one attempt, `/tasks/N` for a task, `/compare?a=3,5` to
compare. Give the user these links when you report progress.
