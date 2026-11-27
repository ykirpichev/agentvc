# agentvc panel for Claude

A pane that sits beside the chat in Claude Code (the Claude desktop app's Code
tab, or the terminal) and shows the review UI's branch tree, live from your
local agentvc space. It is the same tree, words and rules as the web review UI
and the VS Code / Cursor extension:

- **Your stack, like `gt log`.** The window Claude Code runs in: its
  branches with the newest on top and main at the bottom, a filled dot on the
  branch you are on, PR state and clashes as badges, and a line when main
  moved. Other windows are one line each; press one to open its top branch.
- **Sync, Submit stack, Undo** on top: fetch main, drop merged branches and
  rebase the stack; push every branch with one PR each; take back the last
  stack change.
- **Agent work** below: each open task with its tries, named by what they
  did. A task made into a branch shows as an "N tries" badge on it instead.
- **Press a row** for its actions:
  - a branch: Check out, Merge (when its PR can merge), Move onto… (press
    **← here** on a branch or main);
  - a try: Use this, Update from main, Open PR / Update PR, Stop (while an
    agent runs), Discard (with a reason).
- **Clashes** show both sides, titled *Now on <parent>* and *Your change*,
  with Use <parent>, Use mine and Keep both, then Continue or Cancel.
- **Diffs** of the selected branch or try, and links to open it, or the whole
  review UI, in the browser.

Minimized, the pane becomes a one-glance widget: running agents and how many
branches or tries need attention. `/agentvc` opens it again.

```
demo9                                            ● live
[Sync] [Undo]
main-window  this window
↓ main moved 1 commit. Sync to rebase your stack on it.
○ cli-ops  #12 ✓
  ○ div
○ mul
■ main
OTHER WINDOWS
docs  1 branch
AGENT WORK
Format results nicely
  ○ claude  claude · Tests pass
  ○ codex   codex · Tests fail
```

## How it works

It is a Claude Code mod (a plugin with a hooks module). It polls the local
daemon's JSON API (`/api/graph`, `/api/workspaces`, `/api/tasks`, `/api/live`)
every 2 seconds through Claude Code's own HTTP client, so it needs nothing
from the browser and agentvcd's localhost-only guard needs no change. Actions
post to the same endpoints the web UI uses.

The tree is laid out by the web UI's own code: `sync-shared.sh` copies
`ui/src/core/graph.ts`, `core/next.ts`, `core/stack.ts`, `core/words.ts`,
`lib/format.ts` and `api/types.ts`
into `hooks/shared/`, and CI fails when the copies are stale. Change those
files in `ui/src`, then run `sh integrations/claude-desktop/sync-shared.sh`.
Panes have no drag and drop, which is why moves are by press here.

## Install

You need agentvcd running in the repository you work in:

```sh
agentvcd            # serves the space on http://127.0.0.1:7420
```

Then install the plugin from this repository's marketplace. From a shell:

```sh
claude plugin marketplace add ykirpichev/agentvc
claude plugin install agentvc-panel@agentvc
```

or at the prompt of a terminal session:

```
/plugin install agentvc-panel --marketplace ykirpichev/agentvc
```

In the desktop app, where `/plugin` does not run: click **+** next to the
prompt box, then **Plugins**, **Add marketplace** with `ykirpichev/agentvc`,
then **Add plugin**.

The repository is private, so your machine needs a git credential for
github.com (an SSH key, or `gh auth login` then `gh auth setup-git`).

Start a new Code session in the repository. The pane opens by itself once the
daemon answers; `/agentvc` opens it any time.

The pane belongs to Claude Code sessions (the desktop app's Code tab and the
terminal). It cannot add a section to the claude.ai project panel; for that
view see the published artifact.

## Options

Set them in `/config`, or in `~/.claude/settings.json` under
`pluginConfigs["agentvc-panel"].options`:

| Option        | Default                 | What it does                               |
| ------------- | ----------------------- | ------------------------------------------ |
| `url`         | `http://127.0.0.1:7420` | Where agentvcd listens (`agentvcd --port`) |
| `pollSeconds` | `2`                     | How often the pane refreshes               |
| `autoOpen`    | `true`                  | Open the pane at start once the daemon answers |

## Develop

```sh
claude plugin validate integrations/claude-desktop
claude plugin test integrations/claude-desktop     # tests/ against a recorded fixture
claude --plugin-dir integrations/claude-desktop    # run it from the folder; saves hot-reload
```

`tests/fixture.ts` holds answers recorded from agentvcd over a demo space with
two windows. Re-record it when the API shapes change:
`python3 tests/record-fixture.py http://127.0.0.1:7420 <folder holding the demo>`.
