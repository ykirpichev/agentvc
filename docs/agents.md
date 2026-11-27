# Agents: run coding agents on your stacks

`agentvc agent` starts a coding agent on top of any branch of any
workspace's stack, keeps track of it, and puts its work on the stack when
you are happy with it. It is the engine behind the control center in the
review UI (#143). The goal is one person running 10 to 20 agents without
losing track of them.

## The flow

1. Pick a **workspace**, a **branch** of its stack (or main), and optionally
   an older **version** of that branch.
2. Tell the agent what to do:

   ```sh
   agentvc agent start --workspace dates --from iso-dates "Reject dates before 1900, with tests"
   ```

3. The agent works in **its own git worktree on its own branch**, created
   from that point. Your checkout of the workspace is never touched, and
   five agents can start from the same branch.
4. When it stops, agentvc commits what it changed, runs the check command
   and sets its state. `agentvc agent ls` shows what needs you.
5. Review it (`agentvc agent show agN`, or the diff of its branch). Then
   either **land** it or **discard** it:
   - by default the agent's branch joins the stack as a new branch on top
     of the start branch;
   - with `--revise` on `start`, landing makes the agent's commit the start
     branch's new version and restacks the branches above it.

   Landing goes through the workspace's edit queue. If an agent is working
   in that workspace's own checkout, the edit waits until it stops.

## In the review UI

- **Sidebar**: each agent is a line under the branch it started from (under
  main for agents started there), with a dot and its state in plain words.
  **Needs you** at the top counts the agents waiting on you; the browser tab
  shows the count too.
- **Home (Needs you)**: one card per agent that waits on you, most urgent
  first. Answer its question, approve or deny a command, land it, send failing
  checks back, or discard it, right on the card. Agents still working are
  listed below with their last step and the slots in use.
- **Branch page** (click a branch or main): **Start agent** with what to do
  and where the result goes (new branch on top, or a new version of this
  branch), plus the agents started from there.
- **Agent page** (`/review/a/agN`): its conversation, time, cost, checks, its
  changes as diffs and its recent steps, with the same actions.

To try it without Claude Code, `integrations/vscode/scripts/demo-workspaces.sh
DIR BINDIR` then `demo-agents.sh DIR BINDIR` start 11 script agents in every
state across three workspaces.

## States

| State | Shown as | Needs you | What you do |
| --- | --- | --- | --- |
| `asking` | Has a question | 1st | `agentvc agent reply agN "..."` |
| `needs_approval` | Needs your OK | 1st | `agentvc agent approve agN` or `deny agN --reason ...` |
| `failed` | Failed | 2nd | read the error, `reply` to try again, or `discard` |
| `tests_failing` | Tests failing | 3rd | `reply` with what to fix, or review anyway |
| `ready` | Ready to review | 4th (5th when stale) | `land` or `discard` |
| `stopped` | Stopped | 4th | `land`, `reply` to continue, or `discard` |
| `queued` | Waiting for a slot | no | nothing: it starts when a slot frees up |
| `running` | Working | no | watch its last step, or `stop` it |
| `landed`, `discarded` | | no | kept for history (`ls --all`) |

An agent is **stale** when its start branch moved after the agent started.
It can still land as a new branch, and the stack's restack moves it on top.
A revision of a branch that moved is refused with `branch_moved`.

## Runners

- **Claude Code** (default): `claude -p --output-format stream-json`, run in
  the worktree with agentvc's MCP server attached. Its steps, cost and turns
  are recorded as they stream. Two MCP tools reach you:
  - `ask_human`: the agent asks one question and waits for your reply;
  - `approve`: the permission prompt tool. A command the agent may not run
    on its own becomes an Approve / Deny request instead of a failure.

  Edits are allowed (`--permission-mode acceptEdits`). A reply after it
  stopped resumes the same Claude Code session. `AGENTVC_CLAUDE` names
  another `claude` program.
- **Any command**: `--command "<shell>"` runs that instead. The prompt is in
  `$AGENTVC_AGENT_PROMPT` and each output line becomes a step. It can ask
  you a question with `"$AGENTVC_BIN" agent ask "..."`, which prints your
  answer.

## Settings

Agent settings belong to the **workspace**: each workspace says how agents
started in it run, and every agent started there inherits that. Set them on
the workspace's main page in the review UI (click main under the workspace),
or:

```sh
agentvc workspace agents                                   # show
agentvc workspace agents --runner claude --model opus --check "cargo test" --permissions auto --sandbox true --max 3
agentvc workspace agents --workspace dates --runner codex  # another workspace
agentvc workspace agents --clear                           # back to the defaults
```

| Setting | Values | Meaning |
| --- | --- | --- |
| `runner` | `claude` (default), `codex`, `cursor`, `command` | what runs the agent |
| `command` | shell | for `command`: the prompt is in `$AGENTVC_AGENT_PROMPT` |
| `model` | any | passed to the runner |
| `check` | shell | run when an agent stops; failing makes it "Tests failing" |
| `permissions` | `auto` (default), `edits`, `ask`, `bypass` | Claude Code: auto mode (a safety check approves routine steps, risky ones come to you); edit files and ask before commands; ask before every change; never ask (no checks) |
| `sandbox` | `true`, `false` (default) | Claude Code: run commands in its sandbox, so they write only inside the agent's worktree |
| `max` | number | agents working at once in this workspace; the rest wait |

They are kept in the workspace's `stack.json`, so `stack submit` publishes
them with the workspace and other machines see them. A start can override
any of them (`agent start --runner/--model/--check/--permissions`, or "Just
for this agent" in the UI). Unset fields fall back to the repo's
`git config` below, then the defaults. (Through a hosted service, now
paused, nobody can set `command` or `check` (`remote_shell_off`) or choose
`bypass` (`remote_bypass_off`), even with `--allow-remote-agents`.)

### Your setup and what agents may do

Claude Code agents reuse your own setup, so they ask as rarely as your
interactive sessions do. Claude Code reads your user settings
(`~/.claude/settings.json`), your user MCP servers and the repo's checked-in
`.claude/settings.json` and `.mcp.json` itself. Because the agent's worktree
is at another path, agentvc passes on what is keyed by the workspace's
checkout: its `.claude/settings.local.json` (allow rules, hooks, env) and its
entry in `~/.claude.json` (local MCP servers, approved `.mcp.json` servers,
`allowedTools`). `CLAUDE_CONFIG_DIR` is honoured.

Guard rails on every Claude Code agent, whatever the level:

- a `PreToolUse` hook (`agentvc agent guard`) refuses file edits outside the
  agent's worktree or inside `.git`;
- `git push` is denied; agentvc commits and lands the work after review;
- `permissions.additionalDirectories` and `permissions.defaultMode` from your
  local settings are not passed on.

`agentvc agent permissions` (and *What agents may do* in the UI, at
`/settings/agents`, linked from each workspace's agent settings and each
agent's page) shows, per workspace, the level and where it comes from, the
files and rules agents inherit, their MCP servers, and each open agent's
mode as Claude Code reported it.

Codex runs as `codex exec --full-auto` and Cursor as `cursor-agent -p
--force`; `AGENTVC_CODEX` and `AGENTVC_CURSOR` name other programs.

Repo-wide defaults and the machine's cap:

| `git config` | Default | Meaning |
| --- | --- | --- |
| `agentvc.agent-check` | none | check command run when an agent stops, e.g. `cargo test --workspace` |
| `agentvc.agent-runner`, `-model`, `-permissions`, `-sandbox`, `-command` | | defaults for the workspace settings above |
| `agentvc.agents-max` | 8 | safety cap: agents working at once on this machine across all workspaces; the rest wait as `queued` |
| `agentvc.agent-worktrees` | `~/.agentvc/worktrees/<repo>-<hash>` | where agent worktrees go (never under `.git`: Claude Code refuses to edit there) |

## Where things live

Each agent is a directory `<git-common-dir>/agentvc/agents/agN/` with
`agent.json` (the record), `steps.jsonl`, `output.log` and `answers/`. The
CLI, the daemon, the MCP server inside the agent and the agent's worker
process all read and write the same files. Agent worktrees carry a marker
in their git directory, so they never show up as workspaces.

## Daemon API

- `GET /api/agents[?all=1]`: agents newest first, plus `needs_you`,
  `working`, `limit`, `machine` and `can_steer`.
- `POST /api/agents` with `{workspace, from, at?, prompt, mode: "new"|"revise", runner?, model?, check?, permissions?, command?}`.
- `GET /api/workspaces/{id}/agent-settings`: `{settings, defaults, machine_limit}`; `PUT` (or `POST`) the settings to replace them. `/api/workspaces` also carries each workspace's `agent_settings`.
- `GET /api/agents/{id}`: `{agent, steps}`.
- `POST /api/agents/{id}/reply {text}`, `/approve`, `/deny {reason?}`,
  `/stop`, `/land`, `/discard`.
- Event `agent_changed {agent, state}`.

The hosted service is paused; agents are started and answered from the local
UI, editors and CLI. For a self-hosted `agentvc-cloud` (see `deploy/`): calls
that come through it may read agents. Starting,
answering, stopping or landing them from there needs
`agentvcd --cloud ... --allow-remote-agents`, because it runs code on this
machine. Without the flag those calls get `remote_agents_off`, and `GET /api/agents`
says `can_steer: false`, so the hosted UI shows the agents read-only and
says how to allow it.

## Error codes

`prompt_required`, `no_branch`, `bad_ref`, `bad_mode`, `not_in_stack`,
`agent_busy`, `needs_approval`, `no_question`, `reply_required`,
`allow_required`, `not_running`, `not_ready`, `nothing_to_land`,
`branch_moved`, `agent_closed`, `not_an_agent`, `remote_agents_off`,
`bad_setting`, `command_required`, `remote_shell_off`.
