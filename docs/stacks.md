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

### An agent resolves it (the default in the review UI)

Sync, move, delete and landing an agent in the review UI, and
`agentvc stack restack|sync --resolve` on the command line, hand each
conflicting branch to an agent instead of stopping. The other branches restack
as usual; the conflicting one and the branches above it stay where they were
until you accept.

- The agent runs with the workspace's agent settings (runner, model, check
  command, permissions) in its own worktree, never in yours. Its worktree holds
  the branch's commits replayed so far plus the conflicting commit merged with
  diff3 markers, after agentvc's own auto-resolution; the prompt names the
  files, the commit and how to see both sides.
- When it stops, the check command runs. Markers left in a file fail the agent
  (`markers_left`), and it lands nowhere.
- Review it on its agent page: **Accept resolution** (`agentvc agent land agN`)
  commits it with the original message and author, replays the branch's later
  commits, moves the branch and restacks the ones above it (a later clash gets
  an agent of its own). **Redo** is a reply to the agent. **Resolve myself**
  discards it and stops the restack in your worktree, as `--keep-conflicts`.
  A branch that moved since is refused with `branch_moved`.
- Per workspace: `agentvc workspace agents --conflicts manual` (or Conflicts in
  Agent settings, or `git config agentvc.agent-conflicts manual` for the repo)
  makes these stop in the worktree for you instead; `agent` is the default.

Without `--resolve` or `--keep-conflicts` the command line still fails with
`conflict` and changes nothing, so scripts never start an agent by surprise.

## Commands

| Command | Does |
| --- | --- |
| `stack init [--trunk B] [--remote R]` | Set the trunk (detected from `origin/HEAD`) |
| `stack log [--all] [--prs]` | This window's branches with commits, PRs, `needs restack`; `--all` adds other windows read-only; `--prs` fetches each PR's checks and review first |
| `stack create [name] [-m msg] [-a] [-i]` | Branch on the current one (or the trunk), commit staged work; name from the message if omitted. `--insert` puts it between the current branch and its children |
| `stack modify [-a] [-m msg] [-c]` | Amend (or `-c` add a commit), restack the branches above |
| `stack restack [--branch B] [--keep-conflicts \| --resolve]` | Replay branches onto their parents' tips. A commit already in the parent is dropped; one that would turn empty only because conflict resolution took the parent's side (say an indentation-only change) stops with `conflict` instead |
| `stack sync [--no-fetch] [--keep-merged]` | Fetch trunk, drop merged branches, restack. A branch counts as merged only if its tip is in the trunk and it had commits of its own (an empty branch fast-forwarded to trunk is kept), its commits replay onto the trunk as no change without any conflict, or its PR is merged. Before deleting one, its tip is kept as `refs/agentvc/trash/<branch>`: `git branch <branch> refs/agentvc/trash/<branch>` brings it back |
| `stack adopt B [--parent P] [--no-chain]` | Track existing branches: B and the untracked branches below it; parents from Graphite's `refs/branch-metadata` or git history |
| `stack submit [--branch B \| --all] [--draft] [--force] [--no-pr]` | Push with a lease; one PR per branch with its parent as base; the stack listed in every PR body; new PRs get the `agentvc:no-restack` label |
| `stack merge [--branch B] [--method squash\|merge\|rebase] [--dry-run]` | Merge the PRs on GitHub from the bottom up to B (default: current); stops at the first that isn't ready |
| `stack move B --onto P` | Re-parent B (and its upstack) onto P or the trunk; only B's own commits move |
| `stack rename B NEW` | Rename B; branches on it follow (its PR is forgotten, submit opens a new one) |
| `stack delete B` | Delete B and its commits; branches on it move to its parent without them |
| `stack squash [-m msg]` | Squash the current branch into one commit, restack above |
| `stack get TARGET` | Continue a stack published from another machine: TARGET is its workspace id or name, or a branch in it |
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
| `gt merge` (merge the stack) | `agentvc stack merge` |
| `gt log` / `gt ls` | `agentvc stack log` (`--all` for every window) |
| `gt checkout B` | `agentvc stack checkout B` |
| `gt up` / `down` / `top` / `bottom` | same names |
| `gt track` / `untrack` | `agentvc stack adopt` / `untrack` |
| `gt undo` | `agentvc stack undo` |
| `gt get` | `agentvc stack get` |

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

### Checks, review and merging

`agentvc stack log --prs` fetches each open PR's checks, review decision,
open review threads and mergeability, records them in the stack and shows
them on each row (`#12  checks pass, approved`). The daemon refreshes them
for every workspace every 5 minutes, and the review UI's branch tree shows
the same as a badge (`#12 · checks fail`).

`agentvc stack merge` merges the PRs on GitHub from the bottom of the stack
up to `--branch` (default: the current branch), squash by default. Each PR
must be pushed (local tip = PR head), out of draft, mergeable, with passing
checks and no change requests. After each merge it runs `sync` (the merged
branch leaves the stack, the rest restacks onto the trunk), re-submits the
rest so the next PR targets the trunk, then deletes the merged branch on the
remote. The re-submit usually restarts the next PR's checks, so the command
stops there with `stopped.code` `checks_pending`; run it again when they
pass. It never waits. When nothing could merge it fails with the reason as
the error code: `checks_pending`, `checks_failed`, `changes_requested`,
`pr_draft`, `not_mergeable`, `blocked` (branch protection), `not_pushed`,
`no_pr`, `pr_closed`, `merge_failed`; `--dry-run` only says whether the
bottom PR is ready. The branch page in the review UI has a Merge button that
does the same (`{"op": "merge", "branch": "..."}` on
`POST /api/workspaces/{id}/stack`).

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

To keep working on such a stack on the laptop, run `agentvc stack get
<workspace or branch>` in a worktree there: it fetches the branches, creates
them (or moves them to the remote tips) and tracks them in that window with
the published parents and PRs. A branch name brings that branch's whole
stack. Run it again to pick up later pushes; a local branch is replaced only
while it has no commits of its own (else `branch_diverged`, nothing changes).
Then `stack submit` from the laptop pushes with the usual lease. Codes:
`same_machine` (use `give`), `no_workspace`, `ambiguous`, `branch_diverged`.

`ls --json` gives, per branch: `workspace`, `parent`, `parent_kind`
(`trunk|stack|other_window|branch`), `parent_workspace`, `parent_rev`, `tip`,
`commits`, `needs_restack`, `behind` (commits on the base it lacks),
`conflicts` (unmerged files while it is being rebased), `pr {number, state}`,
and with an agentvc space `attempt`, `task`, `status` (`running` or the
attempt's status) for branches exported from attempts.

The daemon serves the same: `GET /api/workspaces[?remote=1]`,
`POST /api/workspaces/{id}/stack` with `{"op": "move", "branch", "onto"}`
(or `restack`, `rename` + `new_name`, `delete`, `sync`, `submit` (the whole
stack, like `stack submit --all`; not undoable); `"queue": true` to
always queue) answering `{"outcome": "applied"|"queued", "request_id"}`, and
`POST /api/requests/{id}/cancel`. `/api/events` adds `stack_changed
{workspace}`, `workspaces_changed`, and `request_queued`, `request_applied`,
`request_failed {reason}`, `request_cancelled`, each with `request_id` and
`workspace`.

Workspace error codes: `no_workspace`, `remote_workspace`, `name_taken`,
`bad_name`, `stack_locked`, `no_request`, `request_not_queued`.

## One space across checkout folders (monorepos)

A monorepo's build often can't run twice in one checkout, so you clone it
into several folders, each with one workspace and its own PR stack. These
folders join one *space*: any of them (`agentvc workspace ls`, its daemon,
the web UI, the VS Code/Cursor panel, the Claude Code pane) lists the
workspaces and stacks of every checkout in the space on this machine, and
can change them. Sync, restack, move, rename, delete, submit, merge,
conflict resolution and agents all run in the checkout that owns the
workspace, honoring its lock and queue.

- By default the space is the checkout's remote URL, normalized
  (`git@github.com:o/r.git` and `https://github.com/o/r` are both
  `github.com/o/r`), so clones of one repo find each other on their own.
- `agentvc space` shows the space and where each workspace lives;
  `agentvc space join KEY` joins another space (git config
  `agentvc.space`); `agentvc space join none` joins none;
  `--reset` goes back to the remote's URL. See [spaces.md](spaces.md).
- `agentvc workspace new DIR [--clone] [--name N]` makes a workspace (a
  worktree, or with `--clone` a separate clone) and says which space it
  joined.
- A checkout joins when it is first listed (any `workspace ls`, or its
  daemon starting). The registry is `~/.agentvc/checkouts.json`
  (`$AGENTVC_HOME` overrides `~/.agentvc`); folders that are gone drop out.
- Branches don't cross clones: `stack give` to a workspace in another
  checkout fails with `other_checkout`. Push the branch and `stack get` it
  there instead.
- Agent ids are unique across the space, so `agentvc agent show agN` works
  from any of its checkouts.

Other machines aren't part of this yet; `workspace publish` and
`ls --remote` still show their stacks read-only.

## Agents on a stack

Any branch of any workspace's stack (or main) can be the starting point of a
coding agent: `agentvc agent start --workspace <name> --from <branch> "..."`
or **Start agent** on the branch page. It works in its own worktree and
branch; landing it adds that branch to the stack on top of its start branch,
or with `--revise` makes it the start branch's new version and restacks the
branches above. Landing goes through the same edit queue as the commands
above, so it waits while an agent works in that workspace's checkout. See
[agents.md](agents.md).
