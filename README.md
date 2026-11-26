# agentvc

Version control built for AI agents. Every agent attempt runs in a cheap
copy-on-write view, failed and competing attempts stay as queryable history,
and a review UI lets a human compare attempts side by side, pick the winner
and ship it as a clean GitHub PR.

Status: early. Milestone M1 (core model and views) is in progress.

- Design: [docs/design.md](docs/design.md)
- One stack per editor window (instead of Graphite's shared tree), and every workspace visible and editable from anywhere: [docs/stacks.md](docs/stacks.md)
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
agentvc fork --from-worktree --move           # stash: the checkout's uncommitted work becomes an attempt
agentvc apply a2                              # pop: write a2's changes into the checkout, uncommitted
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
agentvc log --task t1 --touched src/parser.rs --meta tests=pass
                                              # query: which attempts touched it and passed? (--status,
                                              # --actor, --model, --since 2d, --grep; --json gives flat rows)
agentvc blame src/parser.rs a2                # per line: the snapshot, attempt and agent that wrote it
agentvc op log                                # every operation, with ids
agentvc undo                                  # promoted the wrong one? reopen a2 and a3 (undo again to walk back)
agentvc tag known-good a2 --note "pre-refactor" # name a checkpoint; use it as a ref, gc keeps it
agentvc rebase a2                             # main moved? merge a2 onto origin/main first
agentvc story a2 --at s4:"Add parser" --last "Wire it up"
                                              # split a2 into a stack of commits at snapshot s4
agentvc story a2 --absorb                     # fold review fixes into the change that last touched each file
agentvc export a2 --branch parser-2x          # a2 as a commit (or its story's commits) on a branch
agentvc pr a2 --draft                         # push agentvc/a2 and open a PR listing rejected attempts
agentvc pr --refresh a2                       # PR checks, reviews, comments and the merge back into a2
agentvc comments a4                           # in a follow-up fork: the review to address, with file:line
agentvc land a2 [--onto main] [--merge]       # no GitHub? put a2 on a local branch, then `git push`
agentvc config export.verify true             # run the repo's pre-commit/commit-msg hooks before export/pr/land
agentvc sync                                  # push the whole space (losers too) to origin; merges what
                                              # another machine synced (--prefer ours|theirs on a clash)
agentvc config user.name "Ana Lee"            # this machine's person: actions record human:ana-lee
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

### Telling the work as a series of commits

By default an attempt exports as one squashed commit. `agentvc story` turns it
into a stack of logical changes instead: `--at sN:TITLE` (repeatable, in
order) ends a change at a snapshot and `--last TITLE` names the one ending at
the attempt's head. Each change gets a stable id (`cmnrvpxls`, like jj's change
ids) that survives `agentvc rebase`, which restacks the story onto the new base
with `git merge-tree`. Work recorded after the story is *pending*: `--absorb`
puts each pending file into the last change that touched it (new files go to
the top change), `--append TITLE` adds a change on top, and `export`/`pr`
refuse with `story_stale` until one of them runs (or `--squash` exports one
commit). `export` and `pr` then write one commit per change with an
`Agentvc-Change: <id>` trailer, list them in a "Story" section of the PR body,
and with `--stack` point `<branch>-1`, `<branch>-2`, ... at the lower changes.
`agentvc story aN --check "cargo test"` runs a command at every change in a
scratch fork (warm build output, closed afterwards) and exits 1 if any fails.
`--retitle CHANGE TITLE` renames a change and `--clear` drops the story.

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
  On macOS each top-level directory is cloned whole with one `clonefile(2)`,
  so a fork costs a few system calls however big `node_modules` or `target/` is.
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

### macOS and Windows

CI runs the whole test suite on Linux, macOS and Windows (#78). On macOS
(APFS) forks use `clonefile`; overlayfs is Linux-only and its tests are
compiled out elsewhere. Windows forks with `copy`. There, as git does with
`core.fileMode=false` and `core.symlinks=false`, a file keeps the executable
bit and a symlink checked out as a plain file stays a symlink in snapshots,
taken from the attempt's previous snapshot (or git's index for the first one),
and paths may be given with `\`. Set up a Windows checkout with
`scripts/dogfood-setup.ps1`.

Fork benchmark on macOS (GitHub's `macos-latest` runner, APFS, this repo with a
1.5 GB release `target/`, `scripts/bench-fork.sh . 3`, Nov 2026):

| | time per fork | what the view has |
|---|---|---|
| `agentvc fork` (clonefile) | 110 to 285 ms; 435 ms after editing 10 files in the parent | sources plus the 1.5 GB of ignored build output, shared copy-on-write |
| `git worktree add` | 81 to 105 ms | sources only; a rebuild is still needed |

A forked view starts with a warm build for a fraction of a second more than a
bare worktree.

## Overview for dashboards

`agentvc overview --out overview.json` writes the whole space (tasks, attempt
trees, statuses, reasons, runs, PRs) as one JSON document; the daemon serves the
same at `/api/overview`. The Claude project's **agentvc Experiments** artifact
renders it. `agentvc import --all-prs` first merges every PR's saved space into
the local one, so all PRs' attempt trees show together. See
[docs/overview.md](docs/overview.md).

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
`rebase`, `take`, `query`, `blame`, `sync`, `op_log`, `undo`, `land`, `comments`.
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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you state otherwise, any
contribution you submit for inclusion is dual-licensed the same way, without
additional terms.
