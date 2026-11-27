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
| `stack create [name] [-m msg] [-a] [-i]` | Branch on the current one (or the trunk), commit staged work; name from the message if omitted. `--insert` puts it between the current branch and its children |
| `stack modify [-a] [-m msg] [-c]` | Amend (or `-c` add a commit), restack the branches above |
| `stack restack [--branch B] [--keep-conflicts]` | Replay branches onto their parents' tips |
| `stack sync [--no-fetch] [--keep-merged]` | Fetch trunk, drop merged branches, restack |
| `stack adopt B [--parent P] [--no-chain]` | Track existing branches: B and the untracked branches below it; parents from Graphite's `refs/branch-metadata` or git history |
| `stack submit [--branch B \| --all] [--draft] [--force] [--no-pr]` | Push with a lease; one PR per branch with its parent as base; the stack listed in every PR body; new PRs get the `agentvc:no-restack` label |
| `stack move B --onto P` | Re-parent B (and its upstack) onto P or the trunk; only B's own commits move |
| `stack rename B NEW` | Rename B; branches on it follow (its PR is forgotten, submit opens a new one) |
| `stack delete B` | Delete B and its commits; branches on it move to its parent without them |
| `stack squash [-m msg]` | Squash the current branch into one commit, restack above |
| `stack give B --to DIR` | Hand B and the branches above it to the window at DIR |
| `stack untrack B` | Stop tracking B; branches above keep its commits |
| `stack fold` | Fold the current branch into its parent; branches on it move to the parent |
| `stack split [--by-commit] [--names a,b]` | Give every commit of the current branch but the last its own branch below it (`<branch>-1`, ... by default) |
| `stack absorb [-a]` | Amend each staged file into the branch of this stack whose commits last changed it, restack; other files stay staged |
| `stack checkout B` | Check out a branch of this window (the trunk: detached at `origin/main`) |
| `stack continue` / `stack abort` | After `--keep-conflicts`: finish the rebase and restack the rest, or give up |
| `stack undo [ID] [--list]` | Reverse the latest stack command (repeat to walk back); undoing an undo redoes it |
| `stack up / down / top / bottom` | Move along this window's stack |

Every command takes `--json`. Error codes: `needs_restack`, `push_rejected`,
`parent_not_pushed`, `no_remote`, `same_window`, `other_repository`, `not_in_stack`,
`owned_by_other_window`, `checked_out_elsewhere`, `conflict`,
`rebase_in_progress`, `merge_commit`, `at_top`, `at_bottom`, `ambiguous`,
`no_trunk`, `parent_missing`, `branch_missing`, `message_required`,
`nothing_to_split`, `bad_names`, `nothing_staged`, `nothing_to_absorb`,
`dirty_worktree`, `no_conflicts`, `unresolved_conflicts`, `no_undo`,
`already_undone`, `undo_conflict`. Workspace codes are listed under
[Workspaces](#workspaces-every-window-from-anywhere).

## Coming from Graphite

In each worktree, `agentvc stack adopt <top branch of your stack>` tracks the
chain (parents read from gt's metadata where present, else inferred from
history). Stop running `gt` in that worktree: it would still write the shared
tree. `agentvc stack submit` then finds the existing PRs by branch and keeps
them (bases and the stack list in the body), so Graphite's web review keeps
showing them as a stack.

### `gt` to `agentvc stack`

| Graphite | agentvc |
| --- | --- |
| `gt create -am msg` | `agentvc stack create -am msg` |
| `gt create --insert` | `agentvc stack create -i` |
| `gt modify -a` / `-c` | `agentvc stack modify -a` / `-c` |
| `gt absorb` | `agentvc stack absorb` (by file, not hunk) |
| `gt fold` | `agentvc stack fold` |
| `gt split --by-commit` | `agentvc stack split` |
| `gt squash` | `agentvc stack squash` |
| `gt move --onto B` | `agentvc stack move <branch> --onto B` |
| `gt rename` / `gt delete` | `agentvc stack rename` / `delete` |
| `gt restack` | `agentvc stack restack` |
| `gt continue` / `gt abort` | `agentvc stack continue` / `abort` |
| `gt sync` | `agentvc stack sync` (never moves local `main`) |
| `gt submit --stack` | `agentvc stack submit` |
| `gt log` / `gt ls` | `agentvc stack log` (`--all` for every window) |
| `gt checkout B` | `agentvc stack checkout B` |
| `gt up` / `down` / `top` / `bottom` | same names |
| `gt track` / `untrack` | `agentvc stack adopt` / `untrack` |
| `gt undo` | `agentvc stack undo` |

`stack undo` keeps the last 50 changing commands per worktree
(`<git-dir>/agentvc/stack-undo.json`), including edits made from the review
UI or another window (`--workspace`); the UI's toasts offer Undo for them.
It refuses with `undo_conflict` when a branch the command touched has moved
since. Checkouts, `submit` and `give` are not recorded.

## Pull requests

`submit` pushes the branches that changed, all at once (`--atomic`), with a
lease on what this window last pushed: if anyone else pushed to one of them
(a teammate, or the restack Action), nothing is pushed and the command fails
with `push_rejected`. It opens a PR for each branch that has none (title and
body from its commits; base = the parent branch, or the trunk), retargets
existing ones whose parent changed, and keeps a list of the stack between
`<!-- agentvc:stack:begin -->` markers in every body; the rest of the body is
left alone.

New PRs get the `agentvc:no-restack` label, so the restack Action
(`.github/workflows/restack.yml`) leaves them to `stack sync` in their window.

`sync` also drops branches whose PR GitHub reports merged (when a token is
available and the local branch is what was pushed), so a PR that was edited on
GitHub before merging is still recognized.

## Workspaces: every window from anywhere

Each worktree is a *workspace* with an id (`w1a2b3c4d`) and a name (its
directory name until `agentvc workspace name review`). Any window, agent or
the review UI can see all of them and change any one's stack; the change is
made by and in the workspace that owns the branch, so stacks stay
independent.

```sh
agentvc workspace ls                     # every workspace: stack, PRs, behind, agent running?
agentvc stack move s3 --onto main --workspace feature    # from any window
agentvc stack restack --workspace feature
agentvc workspace requests               # queued and recent edits
agentvc workspace cancel r1a2b3c4d       # cancel a queued one
```

An edit asked from elsewhere (`move`, `restack`, `rename`, `delete`, `sync`
with `--workspace`) is applied at once when that workspace is idle. While an
agent works there, or a rebase is in progress, it is queued in that
workspace's git dir and applied when the agent stops (the agent hooks mark
the workspace busy from the prompt until `Stop`; Claude Code and Cursor) or at
the workspace's next changing stack command. `stack log` never applies them,
so an editor polling it does not move branches under a working agent. A
queued edit that cannot apply (a conflict, a branch gone) is recorded as
failed with its code. Other agents can say they are working with
`agentvc workspace state agent-running` / `idle`. Changing stack commands take
a per-workspace lock (`stack_locked` after 10 s), so edits never interleave.

`give --to` accepts a workspace name as well as a directory.

`stack submit` also publishes the workspace (record and stack) as
`refs/agentvc/workspaces/<id>`; `agentvc workspace ls --remote` on another
machine fetches those and lists them read-only (`remote_workspace` on edits).
`--no-publish` skips it; a remote that refuses `refs/agentvc/*` only warns.

`ls --json` gives, per branch: `workspace`, `parent`, `parent_kind`
(`trunk|stack|other_window|branch`), `parent_workspace`, `parent_rev`, `tip`,
`commits`, `needs_restack`, `behind` (commits on the base it lacks),
`conflicts` (unmerged files while it is being rebased), `pr {number, state}`,
and with an agentvc space `attempt`, `task`, `status` (`running` or the
attempt's status) for branches exported from attempts.

The daemon serves the same: `GET /api/workspaces[?remote=1]`,
`POST /api/workspaces/{id}/stack` with `{"op": "move", "branch", "onto"}`
(or `restack`, `rename` + `new_name`, `delete`, `sync`; `"queue": true` to
always queue) answering `{"outcome": "applied"|"queued", "request_id"}`, and
`POST /api/requests/{id}/cancel`. `/api/events` adds `stack_changed
{workspace}`, `workspaces_changed`, and `request_queued`, `request_applied`,
`request_failed {reason}`, `request_cancelled`, each with `request_id` and
`workspace`.

Workspace error codes: `no_workspace`, `remote_workspace`, `name_taken`,
`bad_name`, `stack_locked`, `no_request`, `request_not_queued`.
