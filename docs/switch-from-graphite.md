# Day one without Graphite

For a monorepo checked out in several folders (one per build), each folder
working its own stack of PRs. agentvc gives every folder its **own stack**,
shows all of them in **one space**, and lets you hand any branch to an
**agent**. Ordinary git branches and GitHub PRs throughout. Command details:
[stacks.md](stacks.md), [spaces.md](spaces.md), [agents.md](agents.md).

## 1. Install (once per machine, 2 minutes)

```sh
git clone https://github.com/ykirpichev/agentvc ~/src/agentvc
cd ~/src/agentvc && scripts/install.sh --vscode
export GITHUB_TOKEN=...    # repo scope; or have `gh auth token` work
```

`install.sh` puts `agentvc` and `agentvcd` in `~/.cargo/bin` and installs the
VS Code / Cursor extension. Run it again to upgrade.

## 2. Move each folder's stack over (once per folder)

In every folder that has a Graphite stack:

```sh
cd ~/src/mono-1
agentvc stack adopt <top branch of this folder's stack>   # parents come from Graphite's metadata
agentvc stack log --prs                                     # check the tree
agentvc stack submit                                        # finds the existing PRs and keeps them
agentvc workspace name mono-1                               # optional: a short name for the UI
```

From now on don't run `gt` in that folder: it would rewrite the shared tree.
Folders with no stack need nothing. Need another folder? `agentvc workspace
new ~/src/mono-4 --clone` makes one (build folders seeded copy-on-write).

## 3. Open the control room

```sh
cd ~/src/mono-1 && agentvcd            # http://localhost:7420
```

One daemon per machine is enough: the sidebar shows the **space** (your
monorepo), every **workspace** (folder) on this machine with its stack and PR
checks, and the agents under the branches they started from. **Needs you**
at the top is the list of agents waiting on you. In Cursor or VS Code the
agentvc side bar shows the same tree.

## 4. The daily loop

| Graphite | agentvc | In the UI |
| --- | --- | --- |
| `gt create -am msg` | `agentvc stack create -am msg` | |
| `gt modify -a` | `agentvc stack modify -a` | |
| `gt absorb` | `agentvc stack absorb -a` (by file) | |
| `gt submit --stack` | `agentvc stack submit` | Submit stack |
| `gt sync` | `agentvc stack sync` | Sync |
| `gt restack` | `agentvc stack restack` | Restack; clashes settle in place |
| `gt log` | `agentvc stack log --prs` | the sidebar |
| `gt merge` | `agentvc stack merge` (re-run when the next PR's checks pass) | Merge on the branch page |
| `gt undo` | `agentvc stack undo` | Undo in the toast |
| `gt get` | `agentvc stack get <branch or workspace>` | |

To move a branch, drag it onto another branch or onto main in the sidebar
(it waits while an agent works there). To hand a branch and the ones above
it to another folder's stack, a worktree or another clone of the repo on
this machine: `agentvc stack give <branch> --to <folder or workspace>`.

## 5. Hand work to an agent

On any branch page: **Start agent**, type what it should do, pick *New branch
on top* (default) or *Revise this branch*. Or:

```sh
agentvc agent start --from my-branch "Add tests for the parser fix"
```

It works in its own worktree, so your folder is not touched. When it is
ready: read the diff, leave comments on lines and send them back as a review,
or **Land** it onto the stack. `agentvc agent ls` shows the same list as Needs
you. Per-folder settings (model, check command, permissions, how many at
once): the workspace's agent settings, or `agentvc workspace agents`.

## Checklist for the trial week

- [ ] `scripts/install.sh --vscode` on each machine, `GITHUB_TOKEN` set
- [ ] Each folder with a stack: `stack adopt`, `stack log --prs`, `stack submit`
- [ ] `gt` alias removed in those folders
- [ ] `agentvcd` running; every folder shows up in the browser and in Cursor
- [ ] First merge with `agentvc stack merge` (or Merge), then `stack sync`
      in the other folders
- [ ] One agent started from a branch, reviewed and landed
- [ ] Friction noted as a GitHub issue labelled `dogfood`

## Known differences from Graphite

- `absorb` works by file, not by hunk.
- `merge` never waits: it stops at the first PR whose checks are still
  running; run it again later.
- `sync` never moves your local `main` (another folder may have it checked
  out); stacks are based on `origin/main`.
- Nothing is interactive: every command takes arguments and `--json`, so
  agents can run them too.

Want several agents to race on the same task and pick the best? That is
the attempt engine under agents: see [attempts.md](attempts.md). You don't
need it for the trial.
