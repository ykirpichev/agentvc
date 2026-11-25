# agentvc

Version control built for AI agents. Every agent attempt runs in a cheap
copy-on-write view, failed and competing attempts stay as queryable history,
and a review UI lets a human compare attempts side by side, pick the winner
and ship it as a clean GitHub PR.

Status: early. Milestone M1 (core model and views) is in progress.

- Design: [docs/design.md](docs/design.md)
- Core library: [crates/agentvc-core](crates/agentvc-core)
- CLI: [crates/agentvc-cli](crates/agentvc-cli)
- MCP server: [crates/agentvc-mcp](crates/agentvc-mcp)
- Claude Code plugin: [integrations/claude-code](integrations/claude-code)
- Review UI: [ui](ui)
- VS Code / Cursor extension: [integrations/vscode](integrations/vscode)

## Try it

```sh
cargo build --release
alias agentvc=$PWD/target/release/agentvc

cd /path/to/your/repo
agentvc init                                  # snapshot the working dir as attempt a1
agentvc task new "make the parser 2x faster"  # -> t1
agentvc fork --task t1 --actor agent:claude   # -> a2 in .agentvc/views/a2, build output included
agentvc fork --task t1 --actor agent:codex    # -> a3, a sibling attempt
cd "$(agentvc path a2)" && ...                # let an agent work there
agentvc snapshot a2 --meta tests=pass         # record state with metadata
agentvc diff a2 a3                            # files that differ between two attempts
agentvc run --task t1 --actor agent:claude --check "cargo test" -- claude -p "make the parser faster"
                                              # fork + run an agent in the view; check decides passed/failed
agentvc run --detach --task t1 -- <agent cmd> # same, in the background; see `agentvc ps`
agentvc hooks                                 # Claude Code hooks: snapshot after every edit, inside any view
                                              # (or install the Claude Code plugin: integrations/claude-code)
agentvc promote a2                            # pick the winner; a3 becomes abandoned (lost to a2)
agentvc log --task t1                         # the attempt tree
agentvc rebase a2                             # main moved? merge a2 onto origin/main first
agentvc export a2 --branch parser-2x          # a2 as a commit on a branch; worktree untouched
agentvc pr a2 --draft                         # push agentvc/a2 and open a PR listing rejected attempts
```

`agentvc pr` reads a token from `GITHUB_TOKEN`, `GH_TOKEN` or `gh auth token`.
Record `tests` and `cost` with `snapshot --meta` and they show up in the PR body.

Every command takes `--json` and returns one JSON object; errors carry a
stable `error.code`. Benchmark forks against `git worktree` with
`scripts/bench-fork.sh <repo>`.

### How views are made

`fork` and `run` pick the fastest backend that works (`--backend` forces one):

- `reflink` (btrfs, XFS, APFS): every file is cloned, which costs metadata only.
- `overlay` (Linux without reflinks, e.g. ext4): the view is an overlayfs
  mount over frozen layers of the parent. A parent is copied once into a base
  layer (`agentvc init` starts that in the background, `agentvc prepare aN`
  does it on demand); after that, forks write only what changed, so they take
  tens of milliseconds. Mounted with `mount(2)` as root,
  `fuse-overlayfs` if installed, or else inside an unprivileged user
  namespace, where only agentvc processes (and what `agentvc run` starts) see
  the view. Views are remounted on demand after a reboot (`agentvc path aN`).
- `copy`: plain copy, the fallback. `--json` output says why in
  `fork.backend_reason`.

## MCP server

`agentvc mcp` serves the agentvc commands to coding agents over the Model
Context Protocol (stdio). Tools: `task_new`, `task_list`, `fork`, `snapshot`,
`mark`, `abandon`, `promote`, `log`, `show`, `diff`, `path`, `run`, `ps`, `pr`,
`rebase`.
Each one runs the matching command with `--json` and returns the same JSON,
with `isError` set and the stable `error.code` (for example `no_space`) on
failure. Resources: `agentvc://tree` (attempts of the current task) and
`agentvc://attempt/aN` (metadata, snapshots and changed files).

Commands run in the directory the server starts in, or `--space DIR`. That
picks the space and the "current attempt", so start it inside a view to make
`fork` and `snapshot` default to that attempt. Differences from the CLI
defaults: `fork` and `run` use actor `agent`, `snapshot` uses trigger `mcp`,
and `run` detaches unless `detach: false`.

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
cargo build
cargo run -p agentvc-cli -- --help
cargo test --workspace
```
