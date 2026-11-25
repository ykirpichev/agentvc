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
else
  echo "==> agentvc init"
  "$bin" init
fi

# Cloud containers are thrown away: keep the space on origin after every
# run, promote, abandon and pr, so losing attempts survive.
if [ -n "${CLAUDE_CODE_REMOTE:-}" ]; then
  echo "==> agentvc sync --auto (cloud session)"
  if ! "$bin" sync --auto; then
    # Don't warn after every command when the remote refuses the push.
    "$bin" sync --no-auto >/dev/null
    echo "    sync failed, auto-sync left off; paste \`agentvc log\` into the PR before the thread ends"
  fi
fi

echo "==> installing Claude Code hooks"
"$bin" hooks

cat <<MSG

Done. Add this to your shell profile:
  export PATH="$PWD/target/release:\$PATH"
Then follow the loop in docs/dogfooding.md:
  agentvc task new "..." && agentvc run --task t1 -- claude -p "..."
MSG
