#!/usr/bin/env bash
# Builds a demo agentvc space with a few tasks and competing attempts, for
# trying the extension (F5) and taking screenshots.
#   scripts/demo-space.sh /tmp/agentvc-demo [path/to/target/debug]
set -euo pipefail
dir=${1:?usage: demo-space.sh DIR [BIN_DIR]}
bin=${2:-$(cd "$(dirname "$0")/../../.." && pwd)/target/debug}
av() { "$bin/agentvc" --json "$@"; }
view() { av fork --task "$1" --from "$2" --backend copy --actor "$3" --model "$4" --prompt "$5" | sed -E 's/.*"id":([0-9]+),"model".*"view_path":"([^"]*)".*/\1 \2/'; }
snap() { av snapshot "a$1" "${@:2}" >/dev/null; }

rm -rf "$dir" && mkdir -p "$dir/src/parse" "$dir/src/report" "$dir/test" && cd "$dir"
git init -q
cat > src/parse/date.ts <<'TS'
export function parseInvoiceDate(raw: string): Date {
  const [d, m, y] = raw.split('/').map(Number)
  return new Date(y, m - 1, d)
}
TS
cat > src/report/export.ts <<'TS'
import type { Report } from './types'

export function toJson(r: Report): string {
  return JSON.stringify(r, null, 2)
}
TS
printf 'export interface Report { rows: Record<string, string | number>[] }\n' > src/report/types.ts
printf 'import { parseInvoiceDate } from "../src/parse/date"\n\ntest("dd/mm/yyyy", () => {\n  expect(parseInvoiceDate("03/04/2025").getMonth()).toBe(3)\n})\n' > test/date.test.ts
printf 'console.log("legacy logger")\n' > src/log.ts
printf '# invoices\n' > README.md
git add . && git -c user.name=demo -c user.email=demo@example.com commit -qm init
av init >/dev/null

av task new "Fix flaky invoice date parsing" --criteria "date tests pass in every timezone" >/dev/null
read -r a v < <(view t1 a1 agent:claude-code claude-opus "Fix the timezone bug in parseInvoiceDate")
cat > "$v/src/parse/date.ts" <<'TS'
export function parseInvoiceDate(raw: string): Date {
  const m = /^(\d{2})\/(\d{2})\/(\d{4})$/.exec(raw.trim())
  if (!m) throw new Error(`bad invoice date: ${raw}`)
  const [, d, mo, y] = m
  // Invoices are issued in UTC; local midnight shifted dates across zones.
  return new Date(Date.UTC(Number(y), Number(mo) - 1, Number(d)))
}
TS
snap "$a" --trigger tool-call --meta cost_usd=0.21 --meta tokens=18000
cat >> "$v/test/date.test.ts" <<'TS'

test("rejects garbage", () => {
  expect(() => parseInvoiceDate("tomorrow")).toThrow()
})
TS
snap "$a" --trigger run-exit --meta exit_code=0 --meta check_exit_code=0 --meta cost_usd=0.17 --meta tokens=12000 --meta "summary=Parse as UTC with a strict regex; added a failing-input test."
av mark "a$a" passed >/dev/null; best=$a

read -r a v < <(view t1 a1 agent:cursor gpt-5 "Fix the timezone bug in parseInvoiceDate")
cat > "$v/src/parse/date.ts" <<'TS'
import { parse } from 'date-fns'

export function parseInvoiceDate(raw: string): Date {
  return parse(raw, 'dd/MM/yyyy', new Date())
}
TS
snap "$a" --trigger run-exit --meta exit_code=1 --meta check_exit_code=1 --meta cost_usd=0.34 --meta "summary=Switched to date-fns; tests still fail in UTC+13."
av mark "a$a" failed >/dev/null

read -r a v < <(view t1 a1 agent:codex o4 "Fix the timezone bug, keep zero dependencies")
cat > "$v/src/parse/date.ts" <<'TS'
export function parseInvoiceDate(raw: string): Date {
  const [d, m, y] = raw.split('/').map(Number)
  return new Date(Date.UTC(y, m - 1, d))
}
TS
rm "$v/README.md"
snap "$a" --trigger run-exit --meta exit_code=0 --meta check_exit_code=0 --meta cost_usd=0.09 --meta "summary=Minimal fix: build the date in UTC."
av mark "a$a" passed >/dev/null

read -r a v < <(view t1 "a$best" agent:claude-code claude-opus "Also accept ISO dates")
cat >> "$v/src/parse/date.ts" <<'TS'

export function parseAnyDate(raw: string): Date {
  return /^\d{4}-/.test(raw) ? new Date(raw) : parseInvoiceDate(raw)
}
TS
snap "$a" --trigger tool-call --meta cost_usd=0.05

av task new "Add CSV export to reports" --criteria "export.test.ts passes" >/dev/null
read -r a v < <(view t2 a1 agent:claude-code claude-sonnet "Add toCsv next to toJson")
cat >> "$v/src/report/export.ts" <<'TS'

export function toCsv(r: Report): string {
  const cols = [...new Set(r.rows.flatMap((row) => Object.keys(row)))]
  const cell = (v: unknown) => `"${String(v ?? '').replaceAll('"', '""')}"`
  return [cols.join(','), ...r.rows.map((row) => cols.map((c) => cell(row[c])).join(','))].join('\n')
}
TS
printf 'import { toCsv } from "../src/report/export"\n\ntest("quotes cells", () => {\n  expect(toCsv({ rows: [{ a: 1, b: "x\\"y" }] })).toBe("a,b\\n\\"1\\",\\"x\\"\\"y\\"")\n})\n' > "$v/test/export.test.ts"
snap "$a" --trigger tool-call --meta cost_usd=0.12
read -r a v < <(view t2 a1 agent:cursor gpt-5 "Add CSV export using papaparse")
printf '\nexport { unparse as toCsv } from "papaparse"\n' >> "$v/src/report/export.ts"
snap "$a" --trigger tool-call --meta cost_usd=0.04

av task new "Replace console logging with pino" >/dev/null
read -r a v < <(view t3 a1 agent:claude-code claude-opus "Use pino everywhere")
printf 'import pino from "pino"\n\nexport const log = pino()\nlog.info("structured logger")\n' > "$v/src/log.ts"
snap "$a" --trigger run-exit --meta exit_code=0 --meta check_exit_code=0 --meta cost_usd=0.11
av promote "a$a" >/dev/null
echo "demo space ready in $dir"
