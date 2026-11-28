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

On Windows, run `powershell -File scripts/dogfood-setup.ps1` instead (same steps; views
use the copy backend). On macOS the bash script works as is, and forks use APFS clones.

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

`--onto aN` stacks the attempt on version N instead: its base becomes N's latest snapshot
(written as a commit on N's own base, trailer `Agentvc-Stacked-On`), so `diff aX` lists
only what X adds on top of N. When N moves on, run the same command again; the review UI
shows *Update from N*. Dragging a version onto another in the branch graph does this.

On conflicts nothing changes: the command fails with code `conflict` and the paths
(`error.paths` in `--json`). Either hand it back to the agent, or run
`agentvc rebase aX --keep-conflicts`, which writes the files with conflict markers into
the view and marks the attempt `exploring`. Fix them in `$(agentvc path aX)`, then
`agentvc snapshot aX` and `agentvc pr aX`. A promoted attempt can be rebased too; siblings
are never touched.

A follow-up attempt (forked from one whose PR has since merged, squashed or not) is
rebased against that PR's export rather than its old base: main already holds the export,
so only the work done after it is merged in, and lines both versions changed don't
conflict with themselves (#110). This also holds through several fork levels and when
the export that landed was taken after the fork point, e.g. the parent was rebased onto
a newer main and exported again: then the fork point itself is the base, since the
landed export covers it, so what the ancestor added is not added a second time (#133).

A promoted attempt keeps recording its view: fix CI or review comments in it, and
`agentvc snapshot aX` / `agentvc pr aX` pick the fix up (`pr` exports the view as it is
even without a snapshot). Only abandoned attempts are frozen (#116).

### Shipping again from the same branch (#213)

A cloud thread may push only its own branch. After its PR is squash-merged on GitHub,
that branch still points at the old PR head, which the next export (parented on the new
main) does not build on. `agentvc pr` (and `agentvc export`) then checks whether the old
head has *landed*:

- GitHub says so: the PR agentvc last opened from the branch is merged, its head is the
  branch's head, and the export's base contains its merge commit; or
- the content says so: the old head changed something since its merge-base with the
  export's base, and a three-way merge of it into that base is clean and gives exactly
  the base's tree, so every change it made is already there (what a squash merge leaves).

If it has, the export commit gets two parents: the new base first, the old head second.
Its tree is the attempt's own, the branch (and the checkout, when it is checked out)
fast-forwards, the push needs no force, GitHub's diff against main shows only the new
work, and squash-merging it gives one clean commit. A new PR is opened; the merged one is
never updated. The output says `claude/x had merged PR #N; continuing it with a merge
commit (fast-forward)`, and `--json` carries `export.continued_from: {head, pr, via}`
(`via` is `merged_pr` or `contained`).

If the old head has not landed (work on the branch that is not in the base, or a merged
PR the attempt was not rebased past), `pr` refuses with `branch_exists` and says why.
`--force` still moves the branch regardless.

### Conflicts that resolve themselves (#91)

`rebase`, `take` and `merge` resolve the usual conflicts between parallel attempts
before reporting any, deterministically and without an agent: both sides adding at the
same point (an enum variant, a `mod` line, a subcommand, a test, a README section) keeps
both; the same change twice, or a whitespace-only change, keeps the real change; edits to
different words of one line merge token by token; `Cargo.lock` merges by package and
`*.json` by key. What was resolved is listed as `R  path  (strategy, N hunks)` (in
`--json`: `resolved`) and in the snapshot's `auto_resolved` metadata. Hunks that are left
get diff3 markers with the base section (`|||||||`).

Per path, first match wins: `agentvc config merge.strategy '<glob>=<strategy>' --add`,
then `agentvc-merge=<strategy>` in `.gitattributes` (git's `merge=union` counts), then
the defaults. Strategies: `auto`, `union` (keep both sides of every hunk, for
changelogs), `ours`, `theirs`, `json`, `cargo-lock`, `format`, `none`. With
`agentvc config merge.formatter '*.rs=rustfmt --edition 2021' --add`, `auto` formats all
three sides and merges again when hunks are left. `--no-auto-resolve` gives git's result.

`agentvc conflicts replay` re-runs the merge commits in the repository's history with
these rules and says how many conflicts they resolve. On this repository (Nov 26): 8 of
50 merges conflicted in 12 files; 10 files resolve, all with the same lines the human
resolution committed (7 byte for byte, 3 with the two added blocks in the other order).
The 2 left are the same line fixed two different ways.

Promoted or abandoned the wrong attempt, or forked by mistake? `agentvc op log` lists every
operation with its id and `agentvc undo [id]` reverses it (siblings reopen, an undone fork's
view is put aside, not deleted); `agentvc undo <id of the undo>` redoes.

### Conflicts that are left: resolve them without an agent (#92, #93)

`agentvc rebase aX --preview` says which files would merge cleanly, which a strategy
resolves and which conflict, and changes nothing (daemon: `"preview": true`). After
`--keep-conflicts`, each conflicted file is recorded with its three sides:

```sh
agentvc conflicts list a5                  # open conflicts; `agentvc status` shows them too
agentvc conflicts show a5 README.md        # hunks: ours (the attempt), base, theirs (main)
agentvc resolve a5 README.md --take theirs # or ours, both, both-theirs-first, base; --hunk N
```

Editing the markers away by hand (or by an agent) counts too. `resolve` records a
`resolve` snapshot once nothing is open, and `export`/`pr` refuse with
`unresolved_conflicts` while anything is. The review UI uses
`GET /api/attempts/{id}/conflicts` and `POST /api/attempts/{id}/conflicts/resolve`
(`{path, hunks: [{index, take, text?}]}` or `{path, all}`).

### Catch conflicts early: overlapping attempts (#94)

Attempts of different tasks that change the same files will conflict once one lands.
`agentvc overlaps [aN]` lists each such pair with the shared files and marks with `C` the
ones a trial three-way merge (after auto-resolve) says would conflict; `agentvc status`
in a view shows them as `overlap` lines, and the daemon serves `/api/overlaps`. Agents
hear about it as they work: the first time an edit touches a file another open attempt
changed, the `PostToolUse` hook adds a one-line note to the agent's context (once per
file and attempt). Siblings in one task, and an attempt's own ancestors, are not reported.

### Open PRs restack themselves when main moves

`.github/workflows/restack.yml` runs `agentvc restack --all-prs --push --comment` after
every merge to main: each open PR branch is re-merged with the new main using the same
resolution, pushed when clean, or gets a comment naming the files that still conflict.
See [restack.md](restack.md), including the `AGENTVC_RESTACK_TOKEN` secret that lets the
restacked PRs run CI.

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

### Test results and agent metadata (#10, #11, #103)

`agentvc check aN -- <cmd>` snapshots the view, runs the command there and records the
result on that snapshot: pass or fail, exit code, duration, test counts (from cargo test
or pytest output, or `--junit 'target/**/*.xml'`) and the last 40 lines of output (the
whole log stays in `.agentvc/checks/cN.log`). An open attempt is marked passed or failed
(`--no-mark` leaves it); a promoted one keeps its status. The same tree and command reuse
the earlier result, from any attempt (`cached`), unless `--force`. `--name lint` records
other checks next to `tests`. `agentvc run --check` records its check the same way.

A result belongs to a tree. When the attempt changes afterwards (an edit, a review fix, a
`rebase` onto a newer main), `show`, the PR body and the review UI call it stale ("Tests
passed before the latest changes"), so run the check again on the rebased head before
`pr`. That works on a promoted attempt.

`agentvc annotate aN --model m --cost-usd 0.42 --tokens-in N --tokens-out N --note "..."
--transcript path|url` records what the agent cost and said. Each call adds a row; `show`,
`log` and the PR body sum cost and tokens. Claude Code runs record tokens and the
transcript path by themselves at the end of each turn (Stop hook), and
`claude -p --output-format json` adds the cost. The model shows in the PR body, so in
this repository use a family name such as `opus`, not a version. The MCP tools are `annotate` and `check`.

`agentvc log` shows both after each attempt (`tests:pass $0.42`, `?` when stale), and
`log --meta tests=pass` filters on recorded checks.

### Asking the history: filtered log and blame (#73)

`agentvc log` takes filters that combine with AND and then prints flat rows (`--json`:
`rows`, with the task goal, latest metadata and matched files):

```sh
agentvc log --task t3 --touched x.py --meta tests=pass   # touched x.py and passed tests
agentvc log --status failed --actor agent:claude --since 2d
agentvc log --grep "flaky" --touched 'crates/**'
```

`agentvc blame <path> [aN]` goes below the squashed PR commit: each line gets the
snapshot, attempt, actor, model and trigger that last changed it, across the attempts aN
was forked from. Lines older than the attempt (or brought in from main by a rebase) show
the `git blame` of that commit; inside a view, unsnapshotted lines show as `(working)`.
A line that came in with `agentvc take` names its source attempt. The MCP tools are `query`
and `blame`; the daemon serves `GET /api/query` and `GET /api/blame`.

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
     there, and `agentvc check aN -- cargo test --workspace` after each step that works
     (it snapshots, runs the tests and records the result; see "Test results and agent
     metadata" below).
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
   `agentvc check aX -- cargo test --workspace` (a rebase makes the old result stale),
   `agentvc pr aX`. Run fmt and clippy in the view first. `GITHUB_TOKEN` is set in
   cloud sessions. Exporting to the branch checked out in the main checkout (the usual case
   in a thread) moves that checkout along; no detach or `--force` needed.
   Several PRs from one thread branch need no `--force` either (#213): once the first PR
   is squash-merged, the next `agentvc pr aY` (after `agentvc rebase aY`) sees that the
   branch's old head has landed and continues the branch with a merge commit, so the push
   is a fast-forward and a new PR opens with only the new work (see "Shipping again from
   the same branch" below). Never pass `--force` for this; if `pr` refuses with
   `branch_exists`, the branch holds work that is not in main, and the message says so.
6. **PR body:** keep agentvc's generated body; put the required header lines and your
   summary in a file and pass `--body-prefix <file>`, and put the required footer
   (e.g. "🤖 Generated with ..." and the session URL) in another and pass
   `--body-suffix <file>`: it becomes the last lines of the body, below the generated
   part. Or set them once with `agentvc config pr.body-prefix <file>` and
   `pr.body-suffix <file>` (relative to the space) so every `pr` uses them. Text outside
   agentvc's `<!-- agentvc:begin/end -->` markers is kept on every later update, and a
   new suffix replaces the old footer rather than adding another. Say which attempts
   ran and which won. The title is the attempt's own task goal; pass `--title` to set
   another. Later `pr` runs keep a title given with `--title` or edited on GitHub.
   For a big change, tell it as a series of commits before `pr`:
   `agentvc story aX --at sN:"First step" --last "Second step"` (see `agentvc log aX` for
   snapshot ids); `pr` then pushes one commit per change and lists them in the body.
7. **Follow-up work in the same PR** (review fixes): `agentvc pr --refresh aX` pulls the
   checks and review comments (`agentvc status aX` sums them up, `agentvcd` refreshes open
   PRs every 5 minutes), then `agentvc fork --from aX` and `agentvc comments aY` in the new
   attempt lists what to fix with file and line. Edit, snapshot,
   promote the new attempt, `agentvc pr aY` again. The PR body then shows
   the winning path aX -> aY. With a story, the fork inherits it: run
   `agentvc story aY --absorb` (fixes go into the commit that last touched each file) or
   `--append "Address review"` before `pr`.
8. **If agentvc gets in the way**, don't silently fall back to git: file a `dogfood` issue,
   then say in the PR why it didn't go through agentvc.
9. **Before the thread ends:** nothing to do. `agentvc pr` saves the space (on the remote,
   or in cloud sessions in a PR comment, #56/#64), and with auto-sync every later
   `run`, `promote` and `abandon` updates it.

Threads that ship with `agentvc stack` instead need no raw git either:
`agentvc stack adopt <branch>` (fetches the trunk first, so an empty thread branch
starts with 0 commits, #187), `agentvc stack submit` (asks the remote what it has, so a
branch deleted on GitHub is pushed again, #184), and after the PR merged
`agentvc stack sync` (fetches the trunk, prunes tracking refs of deleted remote branches)
then `agentvc stack create <branch> --reset` to restart the thread branch on the latest
trunk. `agentvc stack checkout main` checks out the trunk (detached at `origin/main`).
No `git fetch --prune`, `git checkout main` or `git checkout origin/main`.

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
  If two machines continue the same space, the later sync merges the other's tasks,
  attempts, snapshots and status changes in (#77; short ids may differ per machine, rows
  match by a hidden uid). Only an attempt whose status both changed stops it with
  `sync_conflict` naming the attempt and both values; `--prefer ours|theirs` settles it.
  Set `agentvc config user.name <name>` once per machine so what you do is recorded as
  `human:<name>` (it defaults to `git config user.name` and never syncs).
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
