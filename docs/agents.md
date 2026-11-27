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
  `$AGENTVC_AGENT_PROMPT` and each output line becomes a step. Such agents
  can't ask questions.

## Settings

| `git config` | Default | Meaning |
| --- | --- | --- |
| `agentvc.agent-check` | none | check command run when an agent stops, e.g. `cargo test --workspace` |
| `agentvc.agents-max` | 4 | agents working at once on this machine; the rest wait as `queued` |
| `agentvc.agent-worktrees` | `~/.agentvc/worktrees/<repo>-<hash>` | where agent worktrees go (never under `.git`: Claude Code refuses to edit there) |

## Where things live

Each agent is a directory `<git-common-dir>/agentvc/agents/agN/` with
`agent.json` (the record), `steps.jsonl`, `output.log` and `answers/`. The
CLI, the daemon, the MCP server inside the agent and the agent's worker
process all read and write the same files. Agent worktrees carry a marker
in their git directory, so they never show up as workspaces.

## Daemon API

- `GET /api/agents[?all=1]`: agents newest first, plus `needs_you`,
  `working`, `limit` and `machine`.
- `POST /api/agents` with `{workspace, from, at?, prompt, mode: "new"|"revise", model?, check?, command?}`.
- `GET /api/agents/{id}`: `{agent, steps}`.
- `POST /api/agents/{id}/reply {text}`, `/approve`, `/deny {reason?}`,
  `/stop`, `/land`, `/discard`.
- Event `agent_changed {agent, state}`.

Calls that come through the hosted service may read agents. Starting,
answering, stopping or landing them from there needs
`agentvcd --cloud ... --allow-remote-agents`, because it runs code on this
machine. Without the flag those calls get `remote_agents_off`.

## Error codes

`prompt_required`, `no_branch`, `bad_ref`, `bad_mode`, `not_in_stack`,
`agent_busy`, `needs_approval`, `no_question`, `reply_required`,
`allow_required`, `not_running`, `not_ready`, `nothing_to_land`,
`branch_moved`, `agent_closed`, `not_an_agent`, `remote_agents_off`.
