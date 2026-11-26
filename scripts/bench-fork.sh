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
# Set BACKEND=copy|overlay|reflink to force a view backend. With overlay, the
# first fork from a parent copies it into a base layer; later forks are thin.
# Runs on Linux and macOS (APFS forks use clonefile).
set -euo pipefail

# Milliseconds since the epoch (BSD date has no %N).
now_ms() {
  if [ "$(uname)" = Linux ]; then echo $(( $(date +%s%N) / 1000000 )); else perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; fi
}

src=${1:?usage: bench-fork.sh <repo> [forks]}
n=${2:-5}
bin=${AGENTVC:-$(cd "$(dirname "$0")/.." && pwd)/target/release/agentvc}
[ -x "$bin" ] || { echo "build first: cargo build --release (or set AGENTVC)" >&2; exit 1; }

work=$(mktemp -d)
cleanup() {
  # Overlay views are mounts; unmount them (deepest first) before deleting.
  # Only Linux has overlay views (and /proc/mounts).
  if [ -r /proc/mounts ]; then
    awk -v w="$work/" 'index($2, w) == 1 { print $2 }' /proc/mounts | sort -r |
      while read -r m; do umount "$m" 2>/dev/null || fusermount3 -u "$m" 2>/dev/null || fusermount -u "$m" 2>/dev/null || true; done
  fi
  rm -rf "$work"
}
trap cleanup EXIT
if [ "$(uname)" = Darwin ]; then
  cp -c -R "$src" "$work/repo" 2>/dev/null || cp -R "$src" "$work/repo"   # -c: clonefile on APFS
else
  cp -a --reflink=auto "$src" "$work/repo"
fi
cd "$work/repo"

if [ "$(uname)" = Darwin ]; then
  fs=$(mount | awk -v d="$(df -P . | tail -1 | awk '{print $1}')" '$1 == d { sub(/^\(/, "", $4); sub(/,$/, "", $4); print $4 }')
  tracked=$(du -sh -I .git . | cut -f1)
else
  fs=$(stat -f -c %T .)
  tracked=$(du -sh --exclude=.git . | cut -f1)
fi
ignored_list=$(git status --ignored --porcelain 2>/dev/null | sed -n 's/^!! //p')
ignored=0
[ -n "$ignored_list" ] && ignored=$(echo "$ignored_list" | tr '\n' '\0' | xargs -0 du -sk 2>/dev/null | awk '{s += $1} END {print s + 0}')
echo "filesystem: $fs   repo size (no .git): $tracked   ignored build/deps: $((ignored / 1024)) MB"

"$bin" init --no-prepare >/dev/null
if [ "${BACKEND:-auto}" != copy ]; then
  # What `agentvc init` starts in the background where forks use overlayfs:
  # freeze a1 into a base layer once, off the fork path.
  start=$(now_ms)
  out=$("$bin" --json prepare a1)
  end=$(now_ms)
  if echo "$out" | grep -q '"skipped":null'; then
    echo "agentvc prepare a1 (one-time, in the background after init): $((end - start)) ms"
  fi
fi
echo
echo "agentvc fork (warm, includes ignored dirs):"
for i in $(seq "$n"); do
  start=$(now_ms)
  out=$("$bin" --json fork --from a1 ${BACKEND:+--backend "$BACKEND"})
  end=$(now_ms)
  backend=$(echo "$out" | sed -n 's/.*"backend":"\([a-z]*\)".*/\1/p')
  echo "  fork $i: $((end - start)) ms  backend=$backend"
done
# The parent keeps changing between forks: edit some tracked files, then fork.
git ls-files | head -10 | while read -r f; do [ -f "$f" ] && echo "// edited" >> "$f"; done
start=$(now_ms)
out=$("$bin" --json fork --from a1 ${BACKEND:+--backend "$BACKEND"})
end=$(now_ms)
echo "  fork after editing 10 files in the parent: $((end - start)) ms"
if [ -d .agentvc/layers ]; then
  du -shc .agentvc/layers .agentvc/overlay | tail -1 | awk '{print "  disk used by overlay layers and upper dirs: " $1}'
else
  du -sh .agentvc/views | awk '{print "  disk used by views (apparent, not CoW-aware): " $1}'
fi

echo
echo "git worktree add (cold, no ignored dirs):"
for i in $(seq "$n"); do
  start=$(now_ms)
  git worktree add -q --detach "$work/wt$i" HEAD
  end=$(now_ms)
  echo "  worktree $i: $((end - start)) ms  (+ reinstall and rebuild not measured)"
done
