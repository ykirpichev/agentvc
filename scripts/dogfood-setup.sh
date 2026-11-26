#!/usr/bin/env bash
# Set up this checkout to develop agentvc with agentvc. Safe to run again.
# See docs/dogfooding.md.
set -euo pipefail
cd "$(dirname "$0")/.."

# The UI goes first: release builds of agentvcd embed ui/dist at compile time.
if [ -f ui/package.json ]; then
  echo "==> building the review UI"
  (cd ui && npm ci --silent && npm run build --silent)
fi

echo "==> building agentvc and agentvcd (release)"
cargo build --release -p agentvc-cli -p agentvcd

bin=$PWD/target/release/agentvc
if [ -f .agentvc/meta.db ]; then
  echo "==> space already initialized"
elif "$bin" init --from-remote --this-branch 2>/dev/null; then
  # A new container for the same thread: pick up the attempts the last one
  # synced (agentvc sync, #56).
  echo "==> restored this branch's space from origin"
elif [ -n "${CLAUDE_CODE_REMOTE:-}" ] && "$bin" init --from-pr 2>/dev/null; then
  # Cloud sessions can't push refs/agentvc/*, so the space lives in a comment
  # on this branch's PR (#64).
  echo "==> restored this branch's space from its PR"
else
  echo "==> agentvc init"
  "$bin" init
fi

# Cloud containers are thrown away: keep the space on origin after every
# run, promote, abandon and pr, so losing attempts survive.
if [ -n "${CLAUDE_CODE_REMOTE:-}" ]; then
  # Saves go to the PR comment once `agentvc pr` has run; a refused push
  # is remembered, so later automatic saves don't retry it.
  echo "==> agentvc sync --auto (cloud session)"
  "$bin" sync --auto 2>/dev/null ||
    echo "    nothing saved yet: the space goes into the PR when \`agentvc pr\` opens it"
fi

echo "==> installing Claude Code hooks"
"$bin" hooks

cat <<MSG

Done. Add this to your shell profile:
  export PATH="$PWD/target/release:\$PATH"
Then follow the loop in docs/dogfooding.md:
  agentvc task new "..." && agentvc run --task t1 -- claude -p "..."
MSG
