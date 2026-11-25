# agentvc panel for Claude

A pane that sits beside the chat in Claude Code (the Claude desktop app's Code
tab, or the terminal) and shows your local agentvc space as it changes:

- **Running**: every attempt an agent is working in right now, with its task,
  model, current step, files touched, overlaps with other attempts, the last
  line of its run log, and a **Stop** button for runs started by `agentvc run`.
- **Tasks**: each open task with a count of its attempts by outcome. Click a
  task to expand its **attempt tree**: forks nest under the attempt they came
  from, with status (★ winner, ✓ passed, ✗ failed, ⊘ abandoned, ● running,
  ○ exploring), model, cost, and the agent's summary or the abandon reason.
  Settled tasks (a winner picked, or nothing left open) fold under
  "Show N settled".
- **Review in browser** and **Open review UI** open the agentvcd review UI at
  that task or the inbox for diffs, compare and pick-winner.

Minimized, the pane becomes a one-glance widget: running, active and
needs-review counts. `/agentvc` opens it again.

```
demo                                   ● live
RUNNING · 1
● a8 Add CSV export to reports
  claude-opus · 3m ago                 [Stop]

TASKS · 1 open
▾ t2 Add CSV export to reports
  3 attempts · 1 running
  ├─ ○ a5 exploring claude-opus · $0.45 · 2 files
  │  │ retry with backoff in a5
  │  └─ ● a8 running claude-opus
  └─ ○ a6 exploring claude-sonnet · $0.46 · 2 files
       retry with backoff in a6
Show 1 settled
```

## How it works

It is a Claude Code mod (a plugin with a hooks module). It polls the local
daemon's JSON API (`/api/space`, `/tasks`, `/attempts`, `/live`, `/inbox`, and
`/tasks/{id}/stats` for expanded tasks) every 2 seconds through Claude Code's
own HTTP client, so it needs nothing from the browser and agentvcd's
localhost-only guard needs no change. The only write it makes is
`POST /api/attempts/{id}/stop`.

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

`tests/fixture.ts` holds answers recorded from agentvcd over a small demo
space. Re-record it when the API shapes change.
