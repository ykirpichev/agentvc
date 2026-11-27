#!/usr/bin/env bash
# Demo: agents in every state on top of demo-workspaces.sh's three stacks.
# Usage: demo-agents.sh DIR BINDIR  (after demo-workspaces.sh DIR BINDIR)
# The agents are shell scripts (`agent start --command`), so no Claude Code
# or API key is needed: they ask, wait for approval, fail, pass or fail
# their checks and keep working, like real ones.
set -e
D=$(cd "${1:?dir}" && pwd); B=$(cd "${2:?bindir}" && pwd); AV=$B/agentvc
cd "$D/invoices"
# A stand-in `npm test` for the agents' checks: fails when a font fallback is added.
mkdir -p "$D/bin"
cat > "$D/bin/npm" <<'NPM'
#!/bin/sh
if grep -q fallback src/pdf/fonts.ts 2>/dev/null; then
  echo " FAIL  test/pdf/fonts.test.ts > falls back to Helvetica"
  echo "   Expected font 'Helvetica' to be embedded"
  echo " Tests  1 failed | 11 passed (12)"; exit 1
fi
echo " Tests  12 passed (12)"
NPM
chmod +x "$D/bin/npm"; export PATH="$D/bin:$PATH"
git -C "$D/invoices" config agentvc.agents-max 4
git -C "$D/invoices" config agentvc.agent-worktrees "$D/agents"
start() { # workspace from command check prompt...
  local ws=$1 from=$2 cmd=$3 check=$4; shift 4
  (cd "$D/win-$ws" && $AV agent start --json --from "$from" --command "$cmd" ${check:+--check "$check"} "$@" | sed -n 's/.*"id": *"\(ag[0-9]*\)".*/\1/p' | head -1)
}
step() { printf 'sleep 1; echo "%s"; ' "$@"; }
wait_state() { # id state
  for _ in $(seq 1 60); do
    $AV agent show "$1" --json 2>/dev/null | grep -Eq "\"state\": *\"$2\"" && return 0; sleep 0.5
  done
  echo "warning: $1 never reached $2" >&2
}

# Ready to review: a small change with passing checks.
r1=$(start dates iso-dates "$(step 'Read src/parse/date.ts' 'Edit src/parse/date.ts')printf '\nexport const MIN_YEAR = 1900\n' >> src/parse/date.ts" "npm test" "Reject dates before 1900, with tests")
# Ready, then land it: shows up as a new branch on the stack.
r2=$(start export csv-header "$(step 'Edit src/report/export.ts')printf '\nexport const csvQuote = (s: string) => JSON.stringify(s)\n' >> src/report/export.ts" "npm test" "Quote CSV fields that contain commas")
# Tests failing.
t1=$(start pdf pdf-fonts "$(step 'Read src/pdf/fonts.ts' 'Edit src/pdf/fonts.ts')printf 'export const fallback = \"Helvetica\"\n' >> src/pdf/fonts.ts" "npm test" "Fall back to Helvetica when a font is missing")
# Failed.
f1=$(start export csv-docs "$(step 'Read README.md')sleep 0.3; echo 'error: could not find docs/export.md' >&2; exit 2" "" "Move the CSV docs into docs/export.md")
for a in $r1 $r2 $t1 $f1; do wait_state "$a" '(ready|tests_failing|failed)'; done
$AV agent land "$r2" >/dev/null

# A ready agent whose start branch moved since (stale).
st=$(start pdf pdf-logo "$(step 'Edit src/pdf/images.ts')printf 'export const logoWidth = 120\n' >> src/pdf/images.ts" "npm test" "Scale the logo to 120px")
wait_state "$st" ready
(cd "$D/win-pdf" && printf 'export const logoAlt = "Company logo"\n' >> src/pdf/images.ts && git commit -qam "Alt text for the logo")
# Has a question (holds a slot until answered).
q1=$(start dates utc-dates "$(step 'Read src/parse/date.ts')ans=\$(\"\$AGENTVC_BIN\" agent ask 'Dates like 03/04/2024 are ambiguous. Should I treat them as day/month (Europe) or month/day (US)?'); echo \"Using \$ans\"; printf '\n// order: %s\n' \"\$ans\" >> src/parse/date.ts" "npm test" "Handle ambiguous day/month order in invoice dates")
# Needs your OK.
p1=$(start pdf pdf-preview "$(step 'Read src/pdf/preview.ts')\"\$AGENTVC_BIN\" agent permission --tool Bash --input '{\"command\":\"npm install pdfjs-dist@4\"}' >/dev/null; printf 'export const engine = \"pdfjs\"\n' >> src/pdf/preview.ts" "npm test" "Render the preview with pdf.js")
# Working.
w1=$(start export csv-export "$(step 'Read src/report/export.ts' 'Read src/report/types.ts' 'Edit src/report/export.ts' 'Run npm test -- export')sleep 3600" "npm test" "Stream large CSV exports instead of building one string")
w2=$(start pdf main "$(step 'Read src/pdf/layout.ts' 'Edit src/pdf/layout.ts')sleep 3600" "npm test" "Support US Letter page size")
# Waiting for a slot (4 at once).
w3=$(start dates main "sleep 3600" "npm test" "Add a currency field to invoices")
# Stopped.
s1=$(start dates log-prefix "$(step 'Read src/log.ts')sleep 3600" "npm test" "Log as JSON in production")
wait_state "$q1" asking; wait_state "$p1" needs_approval
sleep 2
$AV agent stop "$s1" >/dev/null 2>&1 || true
$AV agent ls
