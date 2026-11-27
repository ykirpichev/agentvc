# Spaces and workspaces

A **space** is one project that humans and agents work on together. A space
holds **workspaces**: isolated places where one agent or person works without
getting in the others' way. Each workspace is one folder on one machine (a
git worktree or a separate clone) with its own stack of branches, its own
agent settings (`agentvc workspace agents`) and its own queue of edits.

```
space agentvc (s3f9a01c2, github.com/ykirpichev/agentvc)
  machine yury-mac (this machine)
    checkout ~/sources/agentvc
      * agentvc     w1a2b3c4d  ~/sources/agentvc     (yury)
        review-fix  w5e6f7a8b  ~/sources/agentvc-wt  (yury)
    checkout ~/sources/agentvc-2
        build-2     w9c0d1e2f  ~/sources/agentvc-2   (yury)
```

## Identity

- A space is identified by its **key**: git config `agentvc.space` if set,
  else the checkout's remote URL, normalized (`git@github.com:o/r.git` and
  `https://github.com/o/r` are both `github.com/o/r`).
- Its **id** is `s` + the first 8 hex digits of the key's SHA-1. Every
  machine and every person working on the same repo derives the same id
  without talking to anyone, which is what lets spaces span hosts and users
  later (#157).
- Its **name** is what people call it: the key's last part (`agentvc`) until
  someone runs `agentvc space name NAME`. Names live in this machine's
  registry, `~/.agentvc/checkouts.json` (`$AGENTVC_HOME`).
- A **workspace** has a stable id (`w` + 8 hex), a name unique in its space,
  the machine it lives on, its folder, and the OS account that created it.

## Commands

| Command | What it does |
| --- | --- |
| `agentvc space` | This space and every workspace in it, by machine and checkout folder |
| `agentvc space ls` | Every space with a checkout on this machine |
| `agentvc space name [NAME] [--reset]` | Show or set the space's name; the id never changes |
| `agentvc space join [KEY\|none] [--reset]` | Show or change which space this checkout belongs to (was `workspace space`, which still works) |
| `agentvc workspace new DIR [--clone [--space KEY]] [--name N] [--seed auto\|copy\|off] [--seed-path P]` | Make a workspace cheaply (below) and print the space it joined |
| `agentvc workspace ls` | Every workspace of the space with its stack, folder and machine |

All take `--json`. `workspace ls --json` and `GET /api/workspaces` carry
`space_info` (`id`, `name`, `key`, `source`) and `machine`; each workspace
carries `machine`, `path`, `folder` (`~/...`), `this_machine` and `user`.
`GET /api/space` carries the same `project`, plus `machine` and `folder`.

## Several folders of one repo

Every folder is its own workspace, whatever repo it came from:

- Clones of one GitHub repo join one space on their own, whether their
  remote is `git@github.com:o/r.git` or `https://github.com/o/r`. Each clone
  is a separate checkout with its own git dir, config and build.
- `agentvc space join KEY` (or `workspace new --clone --space KEY`) puts a
  clone in another space; it then lists and edits only that space's
  workspaces.
- Worktrees (`git worktree add`, or `workspace new` without `--clone`) are
  workspaces of their clone and always share its space: the space lives in
  the clone's git config, which its worktrees share.
- Workspace names are unique in a space. Each checkout names its own
  workspaces after their folders, so two clones in folders both called
  `mono` would clash; the older keeps `mono`, the newer becomes `mono-2`,
  saved so every checkout agrees.
- A clone joins its space the first time agentvc runs there (any command,
  or its daemon starting). Until then other folders don't know it exists.

## Cheap workspaces

A new workspace costs three things: git objects, the checked-out files,
and the build and dependency folders (`target/`, `node_modules/`, ...). The
last one is usually the largest and the slowest to rebuild. `agentvc
workspace new` keeps all three cheap:

| Cost | Worktree | Clone (`--clone`) |
| --- | --- | --- |
| Git objects | Shared with its clone: nothing copied | Cloned from the current folder on disk: git hardlinks the objects (copies them across filesystems), then the clone is pointed at the real remote and fetches only what is new. Independent afterwards: deleting the source folder is safe |
| Checked-out files | One `git checkout` of the stack base | Same |
| Build and dependency folders | Seeded from the current workspace | Same |

**Seeding.** After the checkout, each build/dependency folder of the
current workspace is copied into the new one with copy-on-write clones:
`clonefile` on macOS (APFS clones a whole folder in one call), reflinks on
Linux btrfs and XFS. Clones share disk blocks until either side writes, so
a 5 GB `target/` costs almost no disk and takes seconds, not minutes. Seeds are
copied after the checkout, so they are newer than the sources and tools
treat them as up to date: cargo should reuse compiled dependencies and
rebuild only the workspace's own crates (their fingerprints include the
folder path);
`node_modules` is ready as is.

- Which folders: `--seed-path` (repeatable), else git config `agentvc.seed`
  (multi-valued, e.g. `git config --add agentvc.seed bazel-out`), else the
  usual ones that exist and git ignores: `target`, `node_modules` (also one
  and two levels down, for monorepo packages), `.venv`, `venv`, `build`,
  `dist`, `.next`, `.turbo`, `.gradle`, `.angular`, `.nuxt`, `.svelte-kit`.
  Tracked files are never seeded.
- `--seed auto` (default) seeds only with copy-on-write and reports the
  rest as not seeded; `--seed copy` copies byte by byte without it (ext4,
  NTFS); `--seed off` starts clean.
- Builds never share one output folder between workspaces: the monorepo
  build can't run twice at once in one folder, and two agents building into
  one `target/` would trample each other. Copy-on-write gives each workspace
  its own folder at the cost of one.
- Shared caches that are safe for concurrent use still help across
  workspaces and machines: sccache for Rust, pnpm's store, Bazel's disk or
  remote cache. agentvc leaves them as they are.

The same copy-on-write machinery makes attempt views (`agentvc fork`)
instant; a workspace is the long-lived, human-sized version of a view.

## In the UIs

The web UI, the VS Code/Cursor panel and the Claude Code pane show the space
on top, then each workspace with its folder and machine. Hover a workspace for
its id, full path, kind (checkout folder or worktree of which clone) and who
created it.

## Not yet

- Workspaces on other machines are still read-only (`workspace publish`,
  `ls --remote`). Safe remote control is #157.
- In the code, `agentvc_core::Space` is still the per-folder attempt
  history (`.agentvc/`); the project-level space lives in
  `agentvc_core::checkouts`.
