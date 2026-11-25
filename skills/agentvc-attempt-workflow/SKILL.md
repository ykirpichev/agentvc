---
name: agentvc-attempt-workflow
description: Work safely inside an agentvc space. Fork an attempt before a risky or alternative approach, record outcomes, and never edit outside your own view. Use whenever the repository has a .agentvc directory, or the user mentions agentvc, attempts, views or the attempt tree.
compatibility: Requires the agentvc CLI on PATH and a git repository with an agentvc space (run `agentvc init` once). Works with any agent that can run shell commands.
metadata:
  author: agentvc
  version: "0.1.0"
---

# Working in an agentvc space

agentvc records every try at a task as an **attempt**: a node in a tree, with
its own copy-on-write directory, its **view**. Your job is to keep the tree
honest: one approach per attempt, outcomes recorded, no edits in the wrong place.

Ids: tasks are `t1`, attempts `a3`, snapshots `s7`. Every command takes `--json`
and prints one JSON object; errors carry a stable `code`.

## Know where you are

- `agentvc status` says which attempt the current directory belongs to, its task,
  what changed since its last snapshot, and whether main moved (`behind`).
- `agentvc log` shows the tree; `agentvc show aN` shows one attempt and its view path.
- The main checkout is the root attempt `a1`. Views live under `.agentvc/views/aN`;
  `agentvc path aN` prints one (and remounts it if needed).

## Fork before a risky or alternative approach

Fork when you are about to try something you may want to throw away: a large
refactor, a second design, a dependency upgrade, a speculative fix.

```sh
agentvc fork --actor agent:<your-name> --prompt "what you will try" --json
```

- It forks from the attempt of the current directory; `--from aN` or `--from sN`
  picks another parent, `--task tN` another task. A fork keeps its parent's task.
- `cd` into the returned `view_path` and use absolute paths inside it for every
  read and edit from then on.
- To try two approaches, fork twice from the same parent, not one from the other.

## Never edit outside your view

- Do not edit the main checkout, `.agentvc/`, or another attempt's view.
- If you notice you have been editing the wrong place, stop and tell the user.

## Record outcomes

- If hooks are installed (Claude Code plugin, `agentvc hooks`, or Cursor via
  `agentvc integrate cursor`), each edit is snapshotted for you. Otherwise run
  `agentvc snapshot` after each step that works.
- At milestones, attach meta: `agentvc snapshot --meta tests=pass --meta note="parser rewritten"`.
- When you know how an attempt went, `agentvc mark aN passed` or `failed`. Marking
  does not close it. Put the reason in a snapshot note, as above.
- Leave `promote`, `abandon`, `rebase` and `pr` to the human unless they ask you.

## Show the human

The review UI (`agentvcd`, default http://localhost:7420) shows the tree:
`/attempts/N` for one attempt, `/tasks/N` for a task, `/compare?a=3,5` to
compare. Give the user these links when you report progress.
