#!/usr/bin/env bash
# Smoke test for the plugin's entry point, without Claude Code: hooks,
# hook de-duplication, a missing binary, and slash command output.
#
#   integrations/claude-code/tests/plugin.sh [path/to/agentvc]
#
# Defaults to target/debug/agentvc (build it first with `cargo build -p agentvc-cli`).
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
root=$(cd "$here/../.." && pwd)
export AGENTVC_BIN=${1:-$root/target/debug/agentvc}
export AGENTVC_UI_URL=http://localhost:1   # nothing listens: exercises the "not running" note
plugin=$here/bin/agentvc-plugin
avc() { "$AGENTVC_BIN" "$@"; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
contains() { grep -qF -- "$2" <<<"$1" || fail "expected '$2' in: $1"; }

cd "$tmp"
git init -q && echo hello >a.txt && git add . && git -c user.name=t -c user.email=t@t commit -qm init
avc init >/dev/null
avc task new "greet" >/dev/null
export HOME=$tmp/home CLAUDE_PROJECT_DIR=$tmp
mkdir -p "$HOME"

# /attempt forks and prints the view and a review link.
out=$("$plugin" slash attempt --task t1 try shouting)
contains "$out" "Attempt: a2"
contains "$out" "Review:  http://localhost:1/attempts/2"
contains "$out" "agentvcd"
view=$(sed -n 's/^View: *//p' <<<"$out")
[ -d "$view" ] || fail "no view at $view"

# A fork of a2 stays on task t1.
out=$("$plugin" slash attempt --from a2)
contains "$out" "Attempt: a3"
contains "$(avc show a3)" "task    t1"

# PostToolUse hook snapshots the edited view even with the shell elsewhere.
echo HELLO >"$view/a.txt"
printf '{"cwd":"%s","tool_name":"Edit","tool_input":{"file_path":"%s/a.txt"}}' "$tmp" "$view" \
  | "$plugin" hook post-tool-use
contains "$(avc show a2)" "tool:Edit"

# Stop hook in the view records a turn boundary.
printf '{"cwd":"%s"}' "$view" | "$plugin" hook stop
contains "$(avc show a2)" "stop"

# With `agentvc hooks` installed in settings, the plugin's hooks stand down.
avc hooks >/dev/null
before=$(avc show a2 | grep -c '^  s')
printf '{"cwd":"%s"}' "$view" | "$plugin" hook stop
after=$(avc show a2 | grep -c '^  s')
[ "$before" = "$after" ] || fail "plugin hook ran next to settings hooks"
avc hooks --uninstall >/dev/null

# A missing binary never fails a hook.
AGENTVC_BIN=/nonexistent "$plugin" hook stop </dev/null 2>/dev/null || fail "hook exited non-zero"

# /attempts, /compare, /promote.
out=$("$plugin" slash attempts t1)
contains "$out" "a2"
contains "$out" "Review: http://localhost:1/tasks/1"
out=$("$plugin" slash compare a2 a3)
contains "$out" "a.txt"
contains "$out" "http://localhost:1/compare?a=2,3"
contains "$out" "a2 view: $view"
out=$("$plugin" slash compare a2)
contains "$out" "usage: /compare"
out=$("$plugin" slash promote a2)
contains "$out" "Review: http://localhost:1/attempts/2"
contains "$out" "agentvc pr a2"
contains "$(avc show a2)" "promoted"
contains "$("$plugin" slash promote)" "usage: /promote aN"

echo "plugin smoke test: ok"
