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
| `agentvc workspace new DIR [--clone] [--name N]` | Make a workspace: a worktree detached at the stack base, or with `--clone` a fresh clone of the remote. Prints the space it joined |
| `agentvc workspace ls` | Every workspace of the space with its stack, folder and machine |

All take `--json`. `workspace ls --json` and `GET /api/workspaces` carry
`space_info` (`id`, `name`, `key`, `source`) and `machine`; each workspace
carries `machine`, `path`, `folder` (`~/...`), `this_machine` and `user`.
`GET /api/space` carries the same `project`, plus `machine` and `folder`.

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
