#!/usr/bin/env bash
# Demo: one repo, bare origin, three workspaces (worktrees) each with its own unmerged stack.
# Usage: demo-workspaces.sh DIR BINDIR [--no-daemon]
# Then: agentvcd -C DIR/invoices (add --cloud URL to show it on the hosted UI).
set -e
D=${1:?dir}; B=${2:?bindir}; AV=$B/agentvc
HERE=$(cd "$(dirname "$0")" && pwd)
pkill -f "agentvcd -C $D" || true
rm -rf "$D"; mkdir -p "$D"
bash "$HERE/demo-space.sh" "$D/invoices" "$B" >/dev/null
cd "$D/invoices" && git config user.name demo && git config user.email demo@example.com
git init -q --bare "$D/origin.git" && git -C "$D/origin.git" symbolic-ref HEAD refs/heads/main
git remote add origin "$D/origin.git" && git branch -M main && git push -q origin main 2>/dev/null; git remote set-head origin main; git fetch -q origin
for w in dates export pdf; do git worktree add -q "$D/win-$w" origin/main 2>/dev/null; done

cd "$D/win-dates"
printf 'export function parseInvoiceDate(raw: string): Date {\n  const [d, m, y] = raw.split("/").map(Number)\n  return new Date(Date.UTC(y, m - 1, d))\n}\n' > src/parse/date.ts
$AV stack create utc-dates -a -m "Parse invoice dates in UTC" >/dev/null
printf '\nexport function parseIso(raw: string): Date {\n  return new Date(raw)\n}\n' >> src/parse/date.ts
$AV stack create iso-dates -a -m "Accept ISO dates" >/dev/null
printf 'export const log = (...a: unknown[]) => console.error("[inv]", ...a)\n' > src/log.ts
$AV stack create log-prefix -a -m "Prefix log lines" >/dev/null
$AV workspace name dates >/dev/null

cd "$D/win-export"
printf '\nexport function toCsv(r: Report): string {\n  return r.rows.map((x) => Object.values(x).join(",")).join("\\n")\n}\n' >> src/report/export.ts
$AV stack create csv-export -a -m "Add CSV export" >/dev/null
printf '\nexport const csvHeader = (r: Report) => Object.keys(r.rows[0] ?? {}).join(",")\n' >> src/report/export.ts
$AV stack create csv-header -a -m "Write a CSV header row" >/dev/null
printf '# invoices\n\nExport reports as JSON or CSV.\n' > README.md
$AV stack create csv-docs -a -m "Document CSV export" >/dev/null
$AV workspace name export >/dev/null

cd "$D/win-pdf"
mkdir -p src/pdf
printf 'export interface Page { width: number; height: number }\nexport const A4: Page = { width: 595, height: 842 }\n' > src/pdf/layout.ts
$AV stack create pdf-layout -a -m "Page layout for PDF invoices" >/dev/null
printf 'export const fonts = ["Inter", "Inter Bold"]\n' > src/pdf/fonts.ts
$AV stack create pdf-fonts -a -m "Embed fonts" >/dev/null
printf 'export const preview = (n: number) => `page ${n}`\n' > src/pdf/preview.ts
$AV stack create pdf-preview -a -m "Preview the first page" >/dev/null
$AV checkout pdf-layout >/dev/null 2>&1 || $AV stack checkout pdf-layout >/dev/null
printf 'export const logo = "logo.png"\n' > src/pdf/images.ts
$AV stack create pdf-logo -a -m "Put the logo in the header" >/dev/null
$AV workspace name pdf >/dev/null

# Someone else lands on main: clashes with log-prefix in the dates workspace.
cd "$D" && git clone -q origin.git tmpclone && cd tmpclone && git config user.name demo && git config user.email d@e.com
printf 'import pino from "pino"\nexport const log = pino()\n' > src/log.ts && git commit -qam "Switch logging to pino" && git push -q origin main
for w in dates export pdf; do git -C "$D/win-$w" fetch -q origin; done
[ "$3" = --no-daemon ] && exit 0
(nohup "$B/agentvcd" -C "$D/invoices" --pr-poll-secs 0 > "$D/daemon.log" 2>&1 &) ; sleep 2; cat "$D/daemon.log"
