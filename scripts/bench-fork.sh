#!/usr/bin/env bash
# Compare forking an attempt view with `agentvc fork` against `git worktree add`.
#
# Usage: scripts/bench-fork.sh <git-repo-with-build-output> [forks]
#
# The repo is copied to a temp dir first (with reflinks where possible) so the
# original is untouched. `agentvc fork` carries ignored build and dependency
# directories into the view; `git worktree add` does not, so the worktree
# numbers exclude the reinstall and rebuild a worktree would still need. The
# size of those ignored directories is printed so you can see what is skipped.
set -euo pipefail

src=${1:?usage: bench-fork.sh <repo> [forks]}
n=${2:-5}
bin=${AGENTVC:-$(cd "$(dirname "$0")/.." && pwd)/target/release/agentvc}
[ -x "$bin" ] || { echo "build first: cargo build --release (or set AGENTVC)" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -a --reflink=auto "$src" "$work/repo"
cd "$work/repo"

fs=$(stat -f -c %T .)
tracked=$(du -sh --exclude=.git . | cut -f1)
ignored=$(git status --ignored --porcelain 2>/dev/null | sed -n 's/^!! //p' | xargs -r du -sc 2>/dev/null | tail -1 | cut -f1)
echo "filesystem: $fs   repo size (no .git): $tracked   ignored build/deps: $((${ignored:-0} / 1024)) MB"

"$bin" init >/dev/null
echo
echo "agentvc fork (warm, includes ignored dirs):"
for i in $(seq "$n"); do
  start=$(date +%s%N)
  out=$("$bin" --json fork --from a1)
  end=$(date +%s%N)
  backend=$(echo "$out" | sed -n 's/.*"backend":"\([a-z]*\)".*/\1/p')
  echo "  fork $i: $(( (end - start) / 1000000 )) ms  backend=$backend"
done
du -sh .agentvc/views | awk '{print "  disk used by views (apparent, not CoW-aware): " $1}'

echo
echo "git worktree add (cold, no ignored dirs):"
for i in $(seq "$n"); do
  start=$(date +%s%N)
  git worktree add -q --detach "$work/wt$i" HEAD
  end=$(date +%s%N)
  echo "  worktree $i: $(( (end - start) / 1000000 )) ms  (+ reinstall and rebuild not measured)"
done
