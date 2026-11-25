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
else
  echo "==> agentvc init"
  "$bin" init
fi

echo "==> installing Claude Code hooks"
"$bin" hooks

cat <<MSG

Done. Add this to your shell profile:
  export PATH="$PWD/target/release:\$PATH"
Then follow the loop in docs/dogfooding.md:
  agentvc task new "..." && agentvc run --task t1 -- claude -p "..."
MSG
