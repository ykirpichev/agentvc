# Dogfooding: developing agentvc with agentvc

From now on, agent work on this repository goes through agentvc. Every agent
works in its own attempt, a human compares attempts in the review UI, and the
winner ships as a GitHub PR with `agentvc pr`. The goal is two weeks of real
use, with friction filed as issues labelled `dogfood`.

## One-time setup (on your dev machine)

```sh
git clone git@github.com:ykirpichev/agentvc.git && cd agentvc
scripts/dogfood-setup.sh        # builds release binaries, agentvc init, hooks, UI build
```

The script is idempotent. It:

1. builds `agentvc` and `agentvcd` in release mode and the UI (`ui/dist`);
2. runs `agentvc init` in the repo (the space lives in `.agentvc/`, ignored by git);
3. installs the Claude Code hooks into `.claude/settings.local.json`, which is copied
   into every view, so any Claude Code session inside a view snapshots per tool call.

Put `target/release` on your `PATH`, or alias `agentvc` to it.

Instead of step 3 you can install the Claude Code plugin
([integrations/claude-code](../integrations/claude-code/README.md)), which adds the MCP
tools, `/attempt`, `/attempts`, `/compare` and `/promote` on top of the same hooks. Its
hooks stand down while the `agentvc hooks` ones are installed, so run
`agentvc hooks --uninstall` after installing it.

## The loop for each piece of work

```sh
# 1. One task per issue
agentvc task new "Inbox screen (#15)" --criteria "see issue #15"      # -> tN

# 2. Two or three agents in parallel, each in its own attempt
agentvc run --detach --task tN --actor agent:claude --model opus \
  --check "cargo test --workspace" -- claude -p "Implement issue #15. Read CLAUDE.md first."
agentvc run --detach --task tN --actor agent:claude --model sonnet \
  --check "cargo test --workspace" -- claude -p "Implement issue #15. Read CLAUDE.md first."
agentvc ps                                                             # who is still running

# 3. Review in the UI
agentvcd                      # http://localhost:7420 -> task tN -> compare attempts
agentvc diff aX aY            # or from the terminal

# 4. Pick the winner and ship it
agentvc promote aX            # siblings become abandoned (lost to aX)
agentvc rebase aX             # only if main moved since aX was forked (see below)
agentvc pr aX                 # exports a clean commit on agentvc/aX and opens the PR
```

### When main has moved: rebase before `pr`

An attempt's changes are relative to the commit it started from. If other PRs merged
while it was open, exporting it as is would revert them. `agentvc rebase aX` fetches
`origin` and three-way merges the attempt onto `origin/main` (or `--onto <rev>`), writes
the result into its view and records a `rebase` snapshot. `export` and `pr` then parent on
the new commit, and `agentvc diff aX` lists only the attempt's own files.

On conflicts nothing changes: the command fails with code `conflict` and the paths
(`error.paths` in `--json`). Either hand it back to the agent, or run
`agentvc rebase aX --keep-conflicts`, which writes the files with conflict markers into
the view and marks the attempt `exploring`. Fix them in `$(agentvc path aX)`, then
`agentvc snapshot aX` and `agentvc pr aX`. A promoted attempt can be rebased too; siblings
are never touched.

### Combining attempts: take the best parts of each (#71)

When a3 has the better tests and a2 the better code, don't copy files by hand:

```sh
agentvc diff a3 --patch                                   # what a3 changed, hunks in order
agentvc take a3 --into a2 --paths 'tests/**' README.md    # whole files, dirs or globs
agentvc take a3 --into a2 --hunks src/lib.rs:2,3          # single hunks, numbered from 1
agentvc merge a3 --into a2                                # everything a3 changed
```

`--into` defaults to the attempt of the current view. The chosen changes (relative to
a3's own base, as `diff a3` shows them) are three-way merged with a2's current view, so
edits to different parts of one file combine. Conflicts behave like `rebase`: code
`conflict` with `error.paths` and nothing changed, or `--keep-conflicts` for markers. a2
gets a `take` (or `merge`) snapshot with `source`, `source_snapshot`, `paths` and `hunks`
in its metadata; a3 is not touched and can still be abandoned later. The PR body then
lists a3 under **Contributors**. In the review UI, Compare shows "Take into aN" on each
changed file of the other columns (aN is the focused column); the MCP tool is `take`.

Human edits follow the same loop: `agentvc fork --task tN --actor human`, then work in
`$(agentvc path aN)`. If you already started in your normal checkout, turn that work into an
attempt with `agentvc fork --from-worktree --task tN --actor human` (add `--move` to reset the
checkout to HEAD, like `git stash`). To try an attempt in your own IDE, `agentvc apply aN`
writes its changes into the checkout as uncommitted changes (`--paths` for some files); it
refuses with `dirty_worktree` rather than overwrite local edits, and never touches the index
or the branch.

## Checklist for every thread (cloud sessions included)

Each thread runs in a fresh container, so set up at the start and ship before the
thread goes idle. Use your thread's own push branch wherever `<branch>` appears.

1. **Set up** (about a minute): `scripts/dogfood-setup.sh && export PATH=$PWD/target/release:$PATH`.
2. **One task per issue:** `agentvc task new "<goal> (#N)" --criteria "<acceptance criteria>"` gives `tN`.
3. **Never edit the main checkout.** All work happens in attempt views:
   - agents: `agentvc run --detach --task tN --actor agent:claude --model <m> --check "<test cmd>" -- claude -p --permission-mode acceptEdits "<prompt>"`.
     Start two when the approach is open; `agentvc ps` shows them. `run` adds the hooks
     with `--settings` itself, since cloud sessions ignore `settings.local.json` (#57).
   - your own edits: `agentvc fork --task tN --actor agent:claude-code` (or `--from aN` to
     continue a winner), then edit only under `$(agentvc path aN)`, run builds and tests
     there, and `agentvc snapshot aN --meta tests=pass` after each step that works.
   - git works inside views (see "Git inside a view" below): `git status`, `git diff`,
     `git log`, `git blame`, pre-commit hooks and IDE git panels. A `git commit` there is
     fine too; it is recorded as a snapshot. Never `git push` from a view: ship with `agentvc pr`.
4. **Compare:** `agentvc diff aX aY --stat`, then `--patch` for the unified diff. Inside a
   view, `agentvc diff --patch` shows unsnapshotted work and `agentvc status` says where you
   are (and `behind` when main moved). Abandon losers with a reason:
   `agentvc abandon aN --reason "..."`.
5. **Ship:** once per thread, `agentvc config pr.branch <branch>` and add any required
   commit trailers with `agentvc config pr.trailer "Key: value" --add`. Then
   `agentvc promote aX`, `agentvc rebase aX` (always, it's a no-op when main hasn't moved),
   `agentvc pr aX`. Run fmt, clippy and tests in the view first. `GITHUB_TOKEN` is set in
   cloud sessions. Exporting to the branch checked out in the main checkout (the usual case
   in a thread) moves that checkout along; no detach or `--force` needed.
6. **PR body:** keep agentvc's generated body; put required attribution lines and your
   summary in a file and pass `--body-prefix <file>` on the first `pr`. Text outside
   agentvc's `<!-- agentvc:begin/end -->` markers is kept on every later update. Say which
   attempts ran and which won.
   For a big change, tell it as a series of commits before `pr`:
   `agentvc story aX --at sN:"First step" --last "Second step"` (see `agentvc log aX` for
   snapshot ids); `pr` then pushes one commit per change and lists them in the body.
7. **Follow-up work in the same PR** (review fixes): `agentvc fork --from aX`, edit, snapshot,
   promote the new attempt, `agentvc pr aY` again. The PR body then shows
   the winning path aX -> aY. With a story, the fork inherits it: run
   `agentvc story aY --absorb` (fixes go into the commit that last touched each file) or
   `--append "Address review"` before `pr`.
8. **If agentvc gets in the way**, don't silently fall back to git: file a `dogfood` issue,
   then say in the PR why it didn't go through agentvc.
9. **Before the thread ends:** nothing to do. `agentvc pr` saves the space (on the remote,
   or in cloud sessions in a PR comment, #56/#64), and with auto-sync every later
   `run`, `promote` and `abandon` updates it.

## Git inside a view (#68)

Every open view has a `.git` file pointing at its own small git directory,
`.agentvc/git/aN`, which shares the host repository's objects, refs and config (like
`git worktree`, but not registered, so `git worktree list` in the host stays clean):

- `HEAD` is the attempt's base commit, detached: the host HEAD at `agentvc init`, or what
  `agentvc rebase` last moved it onto. So `git status` and `git diff` show exactly the
  attempt's changes, the same files as `agentvc diff aN --stat` and what `agentvc pr` will
  export. `git log`, `git blame` and `git show` see the host's history.
- **`git commit` in a view is recorded as a snapshot** (trigger `git-commit`, with the
  commit id and subject in its metadata), because agents commit by habit and refusing only
  makes them work around it. HEAD stays on the new commit as git users expect, and
  `refs/agentvc/views/aN` keeps it from `git gc`. What ships is still the attempt's
  snapshot: `agentvc pr` exports one clean commit, and `agentvc rebase` resets HEAD to the
  new base.
- The recording is a `post-commit` hook. Views use `.agentvc/git/hooks`, which run the
  repository's own hooks first (`core.hooksPath` or `.git/hooks`), so pre-commit hooks keep
  working. This needs `extensions.worktreeConfig` in the host's git config, which agentvc
  sets on the first fork (it changes nothing for the main checkout).
  `agentvc config git.capture-commits false` before forking leaves the host config alone;
  git still works in views and commits are then plain commits.
- Abandoning an attempt removes its link and git directory. Views made before this get
  their link the next time agentvc touches them (`agentvc status` in the view does it).

## Rules while dogfooding

- No agent edits the main checkout directly. Agents start with `agentvc run`, or in a view.
- Every PR to this repo comes from `agentvc pr` unless agentvc itself is broken; if so,
  file a `dogfood` issue first and say why in the PR.
- Keep losing attempts. Abandon them with a reason (`agentvc abandon aN --reason "..."`);
  that history is what the UI is for.
- Once a week, write down what hurt in a `dogfood` issue: slow forks, confusing output,
  missing UI actions, anything that made you reach for plain git.

## What we measure (#23)

- fork time and disk per attempt (`agentvc fork --json` and `agentvc run --json` report the fork stats);
- attempts per task, and how many win;
- time from the first attempt to `promote`, and from `promote` to merged PR.

## First dogfood run (Nov 25, 2026)

This guide and its setup script were themselves shipped through agentvc: task t1 had
two scripted attempts writing the guide (a2, a3), a2 won, a4 continued it with a
`.gitignore` fix, a5 continued a4 with the first fixes below, a6 continued a5 with the proxy fix that
`agentvc pr` itself ran into, and `agentvc pr a6` opened the PR. What it found:

- **Promote treated ancestors as losers.** Promoting a4 marked its own parent a2
  "lost to a4". Fixed: attempts the winner was forked from are now promoted with
  reason "continued in aN"; only real siblings are abandoned.
- **The UI was missing from release builds.** `agentvcd` embeds `ui/dist` at compile
  time, so the setup script must build the UI before cargo. Fixed in the script.
- **`.claude/` showed up in `git status`.** The hook config is machine-local; it is now
  in `.gitignore`.
- **`agentvc pr` failed behind a TLS-intercepting proxy** ("invalid peer certificate:
  UnknownIssuer"): the GitHub client only trusted the built-in Mozilla roots. Fixed: it
  now honors `SSL_CERT_FILE`, like curl and OpenSSL.
- **Forks are slow on ext4.** Each fork copied about 2.7 GB of `target/` and
  `node_modules`: 3 to 16 s. That is the overlayfs work in #3; on APFS, btrfs or XFS
  reflinks make it near-free.

## Second dogfood run (Nov 25, 2026): issue #32 in a cloud session

Until this run, only #31 and #44 went through agentvc; the 13 other PRs since switch-on
were plain git branches from cloud threads, which said they had no space or token. Both
were wrong: setup takes under a minute and `GITHUB_TOKEN` is there. This run: task t1, two
`claude -p` agents (a2 opus, a3 sonnet) in parallel overlay views, both passed `--check`,
a2 won (a3 listed open attempts as rejected), a4 was a probe, a5 continued a2 with this
guide and a `ps` fix, `agentvc pr a5 --branch ...` opened the PR.

Better than plain git: two agents on the same files at once with no worktree juggling or
rebuilds (forks took 0.5 s with a 700 MB `target/` shared), `--check` marking each attempt,
losers kept with a reason, and promote/rebase/pr as one flow with a PR body that tells the
story. What hurt, filed as `dogfood` issues:

- history dies with the cloud container (#56); hooks silently off in cloud sessions (#57);
- `diff` lists files but shows no patch, and views have no `.git` (#58; `.git` in views is #68);
- `pr` from a thread needs a body prefix and a fixed branch (#59);
- no `agentvc status`, and `run --json` lacks the fork stats this guide promised (#60);
  #58 (except `.git` in views), #59 and #60 fixed in PR #65, itself shipped with `agentvc pr`
  onto its checked-out thread branch;
- `ps` showed a just-started run as `lost` and printed whole prompts. Fixed here.

## Cloud sessions: keeping history and hooks

Cloud threads run in throwaway containers, and Claude Code there ignores
`.claude/settings.local.json`. Two things handle that:

- **History (#56).** `agentvc sync` pushes the whole space (tasks, every attempt with its
  status and reason, snapshot trees, runs and their logs) to `origin` as
  `refs/agentvc/spaces/<space id>`. Each space has its own random id, so threads never
  overwrite each other. `agentvc pr` syncs after opening the PR, and `agentvc sync --auto`
  (which `scripts/dogfood-setup.sh` turns on in cloud sessions) also syncs after every
  `run`, `promote` and `abandon`. A failed sync is a warning, never a failed command.
  On another machine, or in the thread's next container, `agentvc init --from-remote`
  rebuilds the space with the same ids: `log`, `show`, `diff` and the UI show the losers
  too, and open attempts get their views back from their latest snapshot. With several
  spaces on the remote it takes the one synced from the checked-out branch, or
  `--space <id>` from `agentvc sync --list`. The setup script tries this first
  (`--this-branch`), so a thread that lost its container picks up where it stopped.
  If two machines continue the same space, the later sync fails with `sync_conflict`
  instead of hiding the other's attempts; `--force` overwrites.
  **Claude Code cloud sessions** may push only their own branch, so the push is refused
  (HTTP 403). There the space goes into the PR instead (#64): `agentvc pr`, and every
  `sync` after it, keeps one collapsed comment on the PR with the whole space and, for
  each attempt, a git patch from its parent attempt (or its base commit). If that nears
  GitHub's 65,536-character limit, the largest patches are left out first and those
  attempts keep only their history. `agentvc init --from-pr [N]` rebuilds the space with
  the same ids, losers' files included, and the setup script does that on its own in the
  thread's next container. Intermediate snapshots keep their metadata but not their
  files. After one refused push, automatic saves stop trying to push.
- **Hooks (#57).** `agentvc run -- claude ...` adds `--settings .agentvc/claude-hooks.json`
  (a file with only agentvc's hooks) unless the command already passes `--settings` or
  `--plugin-dir`, or, outside a cloud session, the view's own settings install the hooks.
  `--no-hooks` turns this off. A Claude Code run that ends with no hook snapshots prints a
  warning (also in `--json` as `warnings`, and in the run log). `agentvc hooks` says how
  to load the hooks by hand when `CLAUDE_CODE_REMOTE` is set.

## Known gaps at switch-on

- Forks copy build output on filesystems without reflinks (ext4); overlayfs is #3.
- The UI has the shell and API; Inbox, attempt tree and Compare land with #15 to #17.
- `agentvc pr` opens one PR per attempt; stacked story-mode PRs are #21.
