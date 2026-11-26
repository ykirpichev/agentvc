# Stacks per window: `agentvc stack` (#107)

Graphite keeps one stack tree per repository, in the git directory that every
worktree shares. With a Cursor or VS Code window per worktree, every window
sees every other window's branches, `gt sync` in one window moves trunk and
deletes branches under the others, and anything crossing windows stops with an
error. `agentvc stack` gives each worktree its own stack instead, while all
windows share one repository and push ordinary branches.

It works in any git worktree; no `agentvc init` needed.

```sh
git worktree add ../feature-x origin/main        # one worktree per window
cd ../feature-x
agentvc stack create parser -a -m "Parse the new format"   # branch + commit
agentvc stack create cli -a -m "Expose it in the CLI"      # stacked on parser
agentvc stack log                                # this window only
agentvc stack bottom && edit && agentvc stack modify -a    # amend, restack above
agentvc stack sync                               # fetch main, drop merged, restack
```

## Rules that keep windows independent

- **State lives in the worktree's own git directory**,
  `$(git rev-parse --absolute-git-dir)/agentvc/stack.json`. Another window
  never reads or writes it (except `log --all`, which only reads).
- **A branch belongs to one window.** Commands rewrite, check out or delete
  only branches this window tracks. `adopt` refuses a branch another window
  tracks (`owned_by_other_window`).
- **The base is the remote trunk** (`origin/main`), never your local `main`,
  which may be checked out in another worktree. `sync` fetches only that ref.
- **Building on another window's branch is allowed, read-only.**
  `agentvc stack adopt mine --parent theirs` (or `create` while on it): restack
  follows `theirs` when that window amends or restacks it, and never moves it.

## Long chains

Each branch records its parent and the parent commit it sits on. Restack
replays exactly that branch's own commits onto the parent's new tip, so chains
of 10+ branches restack correctly after a parent is amended, rebased or
squash-merged. Replay happens in memory (`git merge-tree` plus agentvc's
conflict auto-resolution) and all refs move in one transaction; the checked
out branch moves with `git reset --keep`, so local edits stay. 15 branches
restack in well under a second.

`sync` removes branches merged into the trunk from the bottom up (merge,
squash or rebase merges: a branch whose commits add nothing to the trunk is
merged), deletes them (`--keep-merged` to keep), puts their children on the
trunk, and restacks.

## Conflicts

A conflict changes nothing: the command fails with code `conflict` and
`error.branch`, `error.commit`, `error.paths`. To resolve it in the editor:

```sh
agentvc stack restack --keep-conflicts   # restacks what it can, then git rebase on the conflicting branch
# resolve in Cursor, git add, then
git rebase --continue
agentvc stack restack                    # carries on with the branches above
```

## Commands

| Command | Does |
| --- | --- |
| `stack init [--trunk B] [--remote R]` | Set the trunk (detected from `origin/HEAD`) |
| `stack log [--all]` | This window's branches with commits, PRs, `needs restack`; `--all` adds other windows read-only |
| `stack create [name] [-m msg] [-a]` | Branch on the current one (or the trunk), commit staged work; name from the message if omitted |
| `stack modify [-a] [-m msg] [-c]` | Amend (or `-c` add a commit), restack the branches above |
| `stack restack [--branch B] [--keep-conflicts]` | Replay branches onto their parents' tips |
| `stack sync [--no-fetch] [--keep-merged]` | Fetch trunk, drop merged branches, restack |
| `stack adopt B [--parent P] [--no-chain]` | Track existing branches: B and the untracked branches below it; parents from Graphite's `refs/branch-metadata` or git history |
| `stack untrack B` | Stop tracking B; branches above keep its commits |
| `stack up / down / top / bottom` | Move along this window's stack |

Every command takes `--json`. Error codes: `not_in_stack`,
`owned_by_other_window`, `checked_out_elsewhere`, `conflict`,
`rebase_in_progress`, `merge_commit`, `at_top`, `at_bottom`, `ambiguous`,
`no_trunk`, `parent_missing`, `branch_missing`, `message_required`.

## Coming from Graphite

In each worktree, `agentvc stack adopt <top branch of your stack>` tracks the
chain (parents read from gt's metadata where present, else inferred from
history). Stop running `gt` in that worktree: it would still write the shared
tree. Next: `stack submit` (one PR per branch, parent as base) and a Stack
panel in the VS Code/Cursor extension.
