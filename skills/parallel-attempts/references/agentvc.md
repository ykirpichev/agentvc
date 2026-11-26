# Parallel attempts with agentvc

agentvc keeps every try at a task as an attempt with its own copy-on-write view
of the repository. Forks share build output and dependencies, so the second and
third attempt do not rebuild from scratch, and the whole tree, losers included,
stays reviewable.

## Setup (once per repository, with the user's OK)

```sh
agentvc init           # snapshots the working directory as attempt a1
agentvcd               # optional: review UI at http://localhost:7420
```

## Agent-run attempts

```sh
agentvc task new "<goal>" --criteria "<criteria>" --json     # task.id is N: use tN
agentvc run --detach --task tN --actor agent:claude --prompt "<approach A>" \
  --check "<check>" -- claude -p --permission-mode acceptEdits "<brief + approach A>"
agentvc run --detach --task tN --actor agent:codex --prompt "<approach B>" \
  --check "<check>" -- codex exec "<brief + approach B>"
agentvc ps                       # what is still running
agentvc log --task tN            # the tree, with passed / failed per attempt
```

- `run` forks a new attempt, runs the command inside its view, snapshots the view
  when it exits, and runs `--check` there: exit 0 marks the attempt passed.
- For `claude` commands, `run` adds agentvc's hooks itself so every edit is
  snapshotted (unless the command passes its own `--settings` or `--plugin-dir`).
- `--from aN` forks from another attempt instead of the current directory's one,
  which is how you continue a promising attempt.

## Attempts you work in yourself

```sh
agentvc fork --task tN --actor agent:<name> --prompt "<approach A>" --json   # -> view_path
cd <view_path>          # edit only here, by absolute paths
<check>  && agentvc snapshot --meta tests=pass
agentvc mark aN passed   # or failed
```

## Collecting

```sh
agentvc diff aN --stat            # what aN changed against its fork point
agentvc diff aX aY --stat         # how two attempts differ
agentvc diff aX aY --patch        # the unified diff
agentvc show aN                   # snapshots, meta, view path
```

Compare in the UI: `http://localhost:7420/compare?a=3,5`.

## Shipping (only on the user's instruction)

```sh
agentvc promote aN                       # siblings closed as lost to aN
agentvc abandon aM --reason "<why>"     # for any attempt left open
agentvc rebase aN                         # onto the latest main; no-op if unchanged
agentvc pr aN                             # one clean commit, pushed, PR opened
```

All commands take `--json`; errors carry a stable `code` (for example
`conflict` from `rebase`, with the paths in `error.paths`).
