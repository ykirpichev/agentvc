# Parallel attempts with plain git worktrees

Use this when agentvc is not installed. It works in any git repository.

## Create

Leave the user's checkout as it is. Uncommitted changes there are not part of
the attempts; if they matter, ask the user before doing anything with them.

```sh
repo=$(basename "$(git rev-parse --show-toplevel)")
base=$(git rev-parse HEAD)
root=$(git rev-parse --show-toplevel)/../$repo.attempts
git worktree add -b attempt/<slug>-a "$root/a" "$base"
git worktree add -b attempt/<slug>-b "$root/b" "$base"
```

Use a short slug from the goal. If a branch name exists, add a number.

## Dependencies and builds

Each worktree is a separate directory with no `node_modules`, `target/`, `.venv`
or other build output. Install and build in each before running the check
(`npm ci`, `cargo build`, `uv sync`, ...). On large repositories this is the main
cost of worktrees; agentvc avoids it by sharing build output between forks.

## Run

Give each attempt its absolute worktree path. Headless agents start in it:

```sh
(cd "$root/a" && claude -p "<brief + approach A>") > "$root/a.log" 2>&1 &
(cd "$root/b" && codex exec "<brief + approach B>") > "$root/b.log" 2>&1 &
wait
```

## Collect

```sh
for w in a b; do
  (cd "$root/$w" && <check>; echo "$w exit=$?")
  git -C "$root/$w" add -A && git -C "$root/$w" commit -qm "attempt $w: <approach>"
  git -C "$root/$w" diff --stat "$base" HEAD
done
git diff attempt/<slug>-a attempt/<slug>-b --stat     # how two attempts differ
```

## Ship and clean up (only on the user's instruction)

```sh
git merge --squash attempt/<slug>-b    # or cherry-pick, or open a PR from the branch
git worktree remove "$root/a"          # keeps the branch; delete it only if asked
```
