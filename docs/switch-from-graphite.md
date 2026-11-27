# Switching a project from Graphite to agentvc

agentvc gives every worktree (every Cursor or VS Code window) its own stack,
while all windows share one repository and push ordinary branches and PRs.
This page is the cutover checklist for one project. Command details are in
[stacks.md](stacks.md).

## 1. Install (once per machine)

```sh
git clone https://github.com/ykirpichev/agentvc ~/src/agentvc
cd ~/src/agentvc && scripts/install.sh --vscode
```

This builds the review UI into `agentvcd`, installs `agentvc` and `agentvcd`
into `~/.cargo/bin`, and with `--vscode` builds the VS Code / Cursor extension
and installs it into `code` and `cursor` when they are on `PATH`. It needs
`cargo`, `npm` and `git`. Run it again to upgrade.

For PRs, export a GitHub token with `repo` scope as `GITHUB_TOKEN` (or have
`gh auth token` work): `submit`, `merge`, `log --prs` and the daemon's PR
poller use it.

## 2. Move each worktree's stack over

In every worktree that has a Graphite stack:

```sh
cd ~/src/my-project-feature-x
agentvc stack adopt <top branch of this window's stack>
agentvc stack log                 # check the parents
agentvc stack submit              # finds the existing PRs and keeps them
```

`adopt` reads parents from Graphite's metadata where present. Stop running
`gt` in that worktree from now on: it would still write the shared tree.
Worktrees with no stack yet need nothing: `agentvc stack create -am "..."`
starts one.

New window for new work:

```sh
git worktree add ../my-project-bugfix origin/main
cursor ../my-project-bugfix
```

## 3. Daily loop

| Graphite | agentvc |
| --- | --- |
| `gt create -am msg` | `agentvc stack create -am msg` |
| `gt modify -a` | `agentvc stack modify -a` |
| `gt absorb` | `agentvc stack absorb -a` |
| `gt submit --stack` | `agentvc stack submit` |
| `gt sync` | `agentvc stack sync` |
| `gt log` | `agentvc stack log --prs` |
| `gt merge` | `agentvc stack merge` (re-run when the next PR's checks pass) |
| `gt undo` | `agentvc stack undo` |
| `gt get` (other machine) | `agentvc stack get <branch>` |

The full table is in [stacks.md](stacks.md#gt-to-agentvc-stack).

## 4. Review UI

```sh
cd ~/src/my-project && agentvcd      # http://localhost:7420
```

`agentvcd` works in any git repository: the first time it creates the
`.agentvc/` directory it keeps its data in (git-ignored, well under a second;
`--no-init` turns that off). The branch tree shows every window's stack with
PR checks and review state. Drag a branch to move it, press Restack, resolve
clashes in place, Merge from the branch page, and Undo from the toast. The
VS Code / Cursor extension and the Claude Code pane show the same tree.

## 5. Optional: agents in attempts

For work where you want several agents to try, use attempts on top
(`agentvc run`, compare, promote, `agentvc pr`): see the
[README](../README.md) and [dogfooding.md](dogfooding.md). Stacks work
without them.

## Checklist

- [ ] `scripts/install.sh --vscode` on each machine
- [ ] `GITHUB_TOKEN` set
- [ ] In each worktree with a stack: `agentvc stack adopt <top>`, `stack log`,
      `stack submit`
- [ ] Stop using `gt` in those worktrees (remove the alias if you have one)
- [ ] `agentvcd` running in the project; the stacks show up in the browser and
      in Cursor
- [ ] First merge done with `agentvc stack merge` (or the Merge button), then
      `agentvc stack sync` in the other windows
- [ ] Optional: in GitHub settings, turn on "Automatically delete head
      branches" so merged branches go away without `stack merge`

## Known differences

- `absorb` works by file, not by hunk.
- `merge` never waits: it stops at the first PR whose checks are still
  running. Graphite's merge queue waits for you.
- `sync` never moves your local `main` (another worktree may have it checked
  out); stacks base on `origin/main`.
- No interactive commands (`gt checkout` with a picker, `gt split` by hunk):
  everything takes arguments and has `--json`, so agents can run it too.
