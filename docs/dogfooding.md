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

Human edits follow the same loop: `agentvc fork --task tN --actor human`, then work in
`$(agentvc path aN)`.

## Rules while dogfooding

- No agent edits the main checkout directly. Agents start with `agentvc run`, or in a view.
- Every PR to this repo comes from `agentvc pr` unless agentvc itself is broken; if so,
  file a `dogfood` issue first and say why in the PR.
- Keep losing attempts. Abandon them with a reason (`agentvc abandon aN --reason "..."`);
  that history is what the UI is for.
- Once a week, write down what hurt in a `dogfood` issue: slow forks, confusing output,
  missing UI actions, anything that made you reach for plain git.

## What we measure (#23)

- fork time and disk per attempt (`agentvc run --json` reports the fork stats);
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

## Known gaps at switch-on

- Forks copy build output on filesystems without reflinks (ext4); overlayfs is #3.
- The UI has the shell and API; Inbox, attempt tree and Compare land with #15 to #17.
- `agentvc pr` opens one PR per attempt; stacked story-mode PRs are #21.
