# agentvc

One control room for every checkout of a repo and every coding agent working
in it. Three ideas:

- **Space**: the repository, with all its checkouts on every machine. Clones
  of one repo in separate folders join the same space on their own (by
  remote URL), so a monorepo checked out five times shows up once.
- **Workspace**: one checkout folder or worktree, usually one editor window.
  Each has its **own stack** of branches and PRs, so windows never fight over
  a shared tree the way Graphite's worktrees do.
- **Agent**: a coding agent started on a branch of a workspace. It works in
  its own worktree, asks you when it needs you, and after you review its diff
  (and send comments back) it lands on the stack as a new branch.

Stacks restack themselves when their base moves, and the usual conflicts
resolve on their own; what is left you settle in place in the review UI.

Status: early, used daily on this repository.

## Day one

```sh
curl -fsSL https://raw.githubusercontent.com/ykirpichev/agentvc/main/scripts/get.sh | sh -s -- --vscode
# agentvc, agentvcd and the VS Code / Cursor extension from the latest release;
# Claude Code and by-hand steps: docs/install.md

cd ~/src/my-repo
agentvcd start                             # review UI at http://localhost:7420, in the background
agentvc stack create my-fix -am "Fix it"   # a branch on this workspace's stack
agentvc stack submit                       # push and open one PR per branch
agentvc agent start --from my-fix "Add tests for the fix"   # an agent on top of it
agentvc agent ls                           # who needs you; answer, review and land from the UI
```

Coming from Graphite: [docs/switch-from-graphite.md](docs/switch-from-graphite.md)
is the day-one guide, with the `gt` to `agentvc stack` table.

## Docs

- Stacks, one per workspace: [docs/stacks.md](docs/stacks.md)
- Spaces and workspaces across folders and machines: [docs/spaces.md](docs/spaces.md)
- Agents (start, needs you, review, land, permissions): [docs/agents.md](docs/agents.md)
- Conflicts and auto-restack of open PRs: [docs/restack.md](docs/restack.md)
- Attempt history under the hood (several agents racing on one task,
  compare, promote, export): [docs/attempts.md](docs/attempts.md)
- Design: [docs/design.md](docs/design.md); how this repo is built with
  agentvc: [docs/dogfooding.md](docs/dogfooding.md)

Code: [CLI](crates/agentvc-cli), [daemon and API](crates/agentvcd),
[core](crates/agentvc-core), [MCP server](crates/agentvc-mcp),
[review UI](ui), [VS Code / Cursor extension](integrations/vscode),
[Claude Code plugin](integrations/claude-code), [agent skills](skills).

Every command takes `--json` and returns one JSON object; errors carry a
stable `error.code`. `agentvc -h` lists the everyday commands; `agentvc --help`
also names the attempt history, CI and upkeep commands.

## Side panel in Claude

`integrations/claude-desktop` is a Claude Code plugin that shows the review
UI's branch tree in a pane beside the chat (desktop app Code tab or terminal):
main, each window's stack, and agent versions, with the same actions, move
preview, conflict resolver and diffs as the web and VS Code UI, read live from
agentvcd:

```
/plugin install agentvc-panel --marketplace ykirpichev/agentvc
```

See [its README](integrations/claude-desktop/README.md) for the desktop app
steps and options.

## MCP server

`agentvc mcp` serves agentvc to coding agents over the Model Context Protocol
(stdio). By default it offers the stack and review tools: `stack_log`,
`stack_create`, `stack_modify`, `stack_submit`, `stack_sync`, `stack_restack`,
`review_comments`, `reply_comment`, `resolve_comment`. Inside an agent started
from the control center it offers that agent's own tools instead of the stack
edits (`ask_human`, `approve`, `commit_review`, plus `stack_log` and the review
tools), since agentvc commits and lands its work. `agentvc mcp --attempts` adds
the attempt-tree tools (`task_new`, `fork`, `snapshot`, `mark`, `promote`,
`log`, `show`, `diff`, `run`, `pr`, `rebase`, `take`, ...) and the resources
`agentvc://tree` and `agentvc://attempt/aN`.

Each tool runs the matching command with `--json` and returns the same JSON,
with `isError` set and the stable `error.code` (for example `conflict`) on
failure. Commands run in the directory the server starts in, or `--space DIR`,
which picks the workspace (and, with `--attempts`, the current attempt).

Claude Code: install the plugin, which bundles the MCP server with the hooks,
slash commands and a skill. See
[integrations/claude-code](integrations/claude-code/README.md):

```sh
claude plugin marketplace add ykirpichev/agentvc
claude plugin install agentvc@agentvc
```

Or just the MCP server, run in the repo: `claude mcp add agentvc -- agentvc mcp`.

Claude Desktop (`claude_desktop_config.json`; Desktop has no working
directory, so pass the space):

```json
{
  "mcpServers": {
    "agentvc": {
      "command": "/path/to/agentvc",
      "args": ["mcp", "--space", "/path/to/your/repo"]
    }
  }
}
```

Cursor: run `agentvc integrate cursor` in the space (see below), or add
`.cursor/mcp.json` by hand:

```json
{
  "mcpServers": {
    "agentvc": {
      "command": "agentvc",
      "args": ["mcp", "--space", "${workspaceFolder}"]
    }
  }
}
```

Try it by hand with the MCP inspector:
`npx @modelcontextprotocol/inspector agentvc mcp`.

## Cursor

```sh
agentvc integrate cursor           # in a space; --json for scripts
agentvc integrate cursor --remove  # revert every file it wrote
```

This writes three files under `.cursor/`, merging into any that exist:

- `mcp.json`: the `agentvc` MCP server, so Cursor's agent can list, fork,
  snapshot and mark attempts.
- `hooks.json`: Cursor hooks. `afterFileEdit` runs `agentvc hook
  cursor-after-file-edit`, which snapshots the attempt whose view holds the
  edited file. `stop` records the end of each agent turn. Snapshots show up in
  the review UI within a second.
- `rules/agentvc.mdc`: an always-on rule describing the attempt workflow,
  including that background and parallel agents each fork their own attempt and
  work only in its view.

`--remove` takes out only agentvc's entries, keeps your own MCP servers, hooks
and rules, and deletes files and directories that are then empty. The command
is written as a bare `agentvc` when that is the binary on `PATH`, so the files
can be committed. Otherwise it is written as an absolute path.

To review attempts inside Cursor, install the VS Code extension
([integrations/vscode](integrations/vscode)): Cursor runs VS Code extensions.

## Build

```sh
(cd ui && npm ci && npm run build)   # the review UI; agentvcd embeds ui/dist
cargo build
cargo run -p agentvc-cli -- --help
cargo test --workspace
```

Without the UI build, `agentvcd` serves a page saying the review UI has not
been built. `scripts/install.sh` does both and installs into `~/.cargo/bin`;
for everyday use, prefer the release install above (`get.sh`, `~/.local/bin`).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you state otherwise, any
contribution you submit for inclusion is dual-licensed the same way, without
additional terms.
