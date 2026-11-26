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
- Agent skills (SKILL.md, any agent): [skills](skills)
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
agentvc diff a2 a3 --patch                    # the unified diff (--stat for a summary)
agentvc status                                # in a view: this attempt, its changes, runs, `behind`;
                                              # in the checkout: open tasks and what needs review
agentvc run --task t1 --actor agent:claude --check "cargo test" -- claude -p "make the parser faster"
                                              # fork + run an agent in the view; check decides passed/failed
agentvc run --detach --task t1 -- <agent cmd> # same, in the background; see `agentvc ps`
agentvc hooks                                 # Claude Code hooks: snapshot after every edit, inside any view
                                              # (or install the Claude Code plugin: integrations/claude-code)
agentvc take a3 --into a2 --paths 'tests/**' --hunks src/lib.rs:2
                                              # combine: a3's tests and one hunk into a2 (snapshot credits a3)
agentvc merge a3 --into a2                    # or three-way merge all of a3's changes into a2
agentvc promote a2                            # pick the winner; a3 becomes abandoned (lost to a2)
agentvc log --task t1                         # the attempt tree
agentvc rebase a2                             # main moved? merge a2 onto origin/main first
agentvc export a2 --branch parser-2x          # a2 as a commit on a branch; worktree untouched
agentvc pr a2 --draft                         # push agentvc/a2 and open a PR listing rejected attempts
agentvc sync                                  # push the whole space (losers too) to origin
agentvc init --from-remote                    # elsewhere: rebuild that space, same ids
agentvc init --from-pr 12                     # or rebuild it from the comment `agentvc pr` keeps on PR #12
agentvc doctor [--repair]                     # check objects, db, views, layers, runs, sync; fix what is safe
agentvc gc [--dry-run]                        # pack objects, prune old losers' trees, drop stale views and layers
```

`agentvc pr` reads a token from `GITHUB_TOKEN`, `GH_TOKEN` or `gh auth token`.
It writes its part of the PR body between `<!-- agentvc:begin -->` and
`<!-- agentvc:end -->` markers and keeps any text around them when it updates
the PR; `--body-prefix FILE` puts text above it, and `--trailer "Key: value"`
adds commit trailers. Settings that every `pr` should use go in
`agentvc config` (`pr.branch`, `pr.base`, `pr.trailer --add`, `pr.body-prefix`).
Exporting to the branch that is checked out moves the checkout along with it.
Record `tests` and `cost` with `snapshot --meta` and they show up in the PR body.

Every command takes `--json` and returns one JSON object; errors carry a
stable `error.code`. Benchmark forks against `git worktree` with
`scripts/bench-fork.sh <repo>`.

### Keeping a space healthy and small

`agentvc gc` packs live objects into a git packfile (`.agentvc/objects/pack`,
written with `git pack-objects`), prunes the trees of attempts abandoned more
than `gc.keepAbandonedDays` (30) ago, deletes views of attempts closed more than
`gc.keepViewsDays` (7) ago and overlay layers nothing uses. Rows stay, so
`log`, `show` (which marks `tree pruned`) and the review UI keep the whole
history. Never pruned: the latest snapshot of each attempt, open, promoted or
exported work, fork points, rebase bases, snapshots with `--meta name=...` or
`keep=...`, and, once the space has synced, anything the last sync did not
carry. Snapshots start `gc --auto` in the background past `gc.auto` (4000)
loose objects; it only packs. `agentvc config gc.auto 0` turns it off.
On a 900-snapshot test space, objects went from 25 MB in 4,692 files to
1.6 MB in one pack; `log` and `diff --patch` took the same time before and after.

`agentvc doctor` checks the database, every object reachable from a snapshot
(present and hashing to its id), views of open attempts, orphan views,
overlay dirs and layers, runs whose process is gone, rebase bases missing from
git, and the sync ref on the remote (`--offline` skips that). Findings have
stable codes and severities in `--json`; it exits non-zero while errors
remain. `--repair` remounts views, rewrites missing ones from their latest
snapshot, copies missing objects back from the host git repo, marks dead runs
lost and deletes orphans.

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

## Overview for dashboards

`agentvc overview --out overview.json` writes the whole space (tasks, attempt
trees, statuses, reasons, runs, PRs) as one JSON document; the daemon serves the
same at `/api/overview`. The Claude project's **agentvc Experiments** artifact
renders it. See [docs/overview.md](docs/overview.md).

## Side panel in Claude

`integrations/claude-desktop` is a Claude Code plugin that shows the attempt
tree and running agents in a pane beside the chat (desktop app Code tab or
terminal), read live from agentvcd:

```
/plugin install agentvc-panel --marketplace ykirpichev/agentvc
```

See [its README](integrations/claude-desktop/README.md) for the desktop app
steps and options.

## MCP server

`agentvc mcp` serves the agentvc commands to coding agents over the Model
Context Protocol (stdio). Tools: `task_new`, `task_list`, `fork`, `snapshot`,
`mark`, `abandon`, `promote`, `log`, `show`, `diff`, `path`, `run`, `ps`, `pr`,
`rebase`, `take`.
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
