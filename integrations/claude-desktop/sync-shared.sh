#!/bin/sh
# Copy the review UI's shared code (graph layout, words, API types, format
# helpers) into the pane, so the Claude Code pane draws the same rows and says
# the same things as the web and VS Code UI. `--check` fails when a copy is
# stale; CI runs it.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
src="$here/../../ui/src"
dst="$here/hooks/shared"
files="api/types.ts core/graph.ts core/words.ts lib/format.ts"
status=0
for f in $files; do
  if [ "${1:-}" = "--check" ]; then
    if ! cmp -s "$src/$f" "$dst/$f"; then
      echo "stale: integrations/claude-desktop/hooks/shared/$f (run integrations/claude-desktop/sync-shared.sh)" >&2
      status=1
    fi
  else
    mkdir -p "$(dirname "$dst/$f")"
    cp "$src/$f" "$dst/$f"
  fi
done
exit $status
