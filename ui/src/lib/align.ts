// Aligned multi-way diff: N versions of one file, each diffed against a
// common base, cut into blocks over the base so that the same base region
// sits on the same grid row in every column.

/** Replace base lines [start, end) with `lines`. */
export interface Edit {
  start: number
  end: number
  lines: string[]
}

export interface Line {
  kind: 'ctx' | 'add' | 'del'
  text: string
  /** Line number shown in the gutter: new side, or old side for deletions. */
  n: number
}

export type Block =
  /** Base lines no column touched. `offsets[i]` maps base to column i's numbering. */
  | { kind: 'same'; start: number; end: number; offsets: number[] }
  /** A base region at least one column changed; one line list per column. */
  | { kind: 'change'; start: number; end: number; cols: Line[][]; changed: boolean[] }

export function splitLines(text: string): string[] {
  if (text === '') return []
  const lines = text.split('\n')
  if (lines[lines.length - 1] === '') lines.pop()
  return lines
}

/**
 * Line edits turning `base` into `next`, using patience diff: lines unique on
 * both sides anchor the alignment (longest increasing run), and the gaps
 * between anchors are diffed recursively. Linear-ish on real code and large
 * regenerated files, where Myers is quadratic in the number of changes.
 */
export function lineEdits(base: string[], next: string[]): Edit[] {
  const ids = new Map<string, number>()
  const id = (l: string) => {
    let v = ids.get(l)
    if (v === undefined) ids.set(l, (v = ids.size))
    return v
  }
  const a = base.map(id)
  const b = next.map(id)
  const out: Edit[] = []
  const emit = (as: number, ae: number, bs: number, be: number) => {
    if (as === ae && bs === be) return
    const last = out[out.length - 1]
    const lines = next.slice(bs, be)
    if (last && last.end === as) {
      last.end = ae
      last.lines.push(...lines)
    } else {
      out.push({ start: as, end: ae, lines })
    }
  }
  diffRange(a, 0, a.length, b, 0, b.length, emit)
  return out
}

type Emit = (as: number, ae: number, bs: number, be: number) => void

/** Regions smaller than this (cells of the LCS table) use exact LCS. */
const LCS_CELLS = 250_000

function diffRange(a: number[], as: number, ae: number, b: number[], bs: number, be: number, emit: Emit): void {
  while (as < ae && bs < be && a[as] === b[bs]) {
    as++
    bs++
  }
  while (as < ae && bs < be && a[ae - 1] === b[be - 1]) {
    ae--
    be--
  }
  if (as === ae || bs === be) return emit(as, ae, bs, be)

  const anchors = uniqueAnchors(a, as, ae, b, bs, be)
  if (anchors.length === 0) {
    if ((ae - as) * (be - bs) <= LCS_CELLS) return lcs(a, as, ae, b, bs, be, emit)
    return emit(as, ae, bs, be)
  }
  let pa = as
  let pb = bs
  for (const [ia, ib] of anchors) {
    diffRange(a, pa, ia, b, pb, ib, emit)
    pa = ia + 1
    pb = ib + 1
  }
  diffRange(a, pa, ae, b, pb, be, emit)
}

/** Pairs of positions of lines that occur exactly once on each side, in an order both sides agree on. */
function uniqueAnchors(a: number[], as: number, ae: number, b: number[], bs: number, be: number): [number, number][] {
  const ca = new Map<number, number>()
  for (let i = as; i < ae; i++) ca.set(a[i]!, ca.has(a[i]!) ? -1 : i)
  const cb = new Map<number, number>()
  for (let j = bs; j < be; j++) if (ca.has(b[j]!)) cb.set(b[j]!, cb.has(b[j]!) ? -1 : j)
  const pairs: [number, number][] = []
  for (let i = as; i < ae; i++) {
    const pa = ca.get(a[i]!)
    const pb = cb.get(a[i]!)
    if (pa === i && pb !== undefined && pb >= 0) pairs.push([i, pb])
  }
  // Longest increasing subsequence on b positions (patience sorting).
  const tails: number[] = []
  const prev = new Array<number>(pairs.length)
  for (let k = 0; k < pairs.length; k++) {
    const v = pairs[k]![1]
    let lo = 0
    let hi = tails.length
    while (lo < hi) {
      const mid = (lo + hi) >> 1
      if (pairs[tails[mid]!]![1] < v) lo = mid + 1
      else hi = mid
    }
    prev[k] = lo > 0 ? tails[lo - 1]! : -1
    tails[lo] = k
  }
  const out: [number, number][] = []
  for (let k = tails.length ? tails[tails.length - 1]! : -1; k >= 0; k = prev[k]!) out.push(pairs[k]!)
  return out.reverse()
}

/** Exact LCS for small regions without unique lines. */
function lcs(a: number[], as: number, ae: number, b: number[], bs: number, be: number, emit: Emit): void {
  const n = ae - as
  const m = be - bs
  const t = new Uint32Array((n + 1) * (m + 1))
  for (let i = n - 1; i >= 0; i--)
    for (let j = m - 1; j >= 0; j--)
      t[i * (m + 1) + j] =
        a[as + i] === b[bs + j]
          ? t[(i + 1) * (m + 1) + j + 1]! + 1
          : Math.max(t[(i + 1) * (m + 1) + j]!, t[i * (m + 1) + j + 1]!)
  let i = 0
  let j = 0
  let ra = 0
  let rb = 0
  while (i < n && j < m) {
    if (a[as + i] === b[bs + j]) {
      emit(as + ra, as + i, bs + rb, bs + j)
      i++
      j++
      ra = i
      rb = j
    } else if (t[(i + 1) * (m + 1) + j]! >= t[i * (m + 1) + j + 1]!) i++
    else j++
  }
  emit(as + ra, ae, bs + rb, be)
}

/** Cut the base into same/change blocks across all columns' edits. */
export function align(base: string[], cols: Edit[][]): Block[] {
  // Merge every column's edit ranges into regions. Touching ranges merge too,
  // so an insert right next to another column's change stays on one row.
  const ranges = cols
    .flat()
    .map((e) => [e.start, e.end] as [number, number])
    .sort((a, b) => a[0] - b[0] || a[1] - b[1])
  const regions: [number, number][] = []
  for (const [s, e] of ranges) {
    const last = regions[regions.length - 1]
    if (last && s <= last[1]) last[1] = Math.max(last[1], e)
    else regions.push([s, e])
  }

  const next = cols.map(() => 0)
  const offsets = cols.map(() => 0)
  const blocks: Block[] = []
  let pos = 0
  for (const [s, e] of regions) {
    if (s > pos) blocks.push({ kind: 'same', start: pos, end: s, offsets: [...offsets] })
    const out: Line[][] = []
    const changed: boolean[] = []
    cols.forEach((edits, i) => {
      const lines: Line[] = []
      let p = s
      let touched = false
      const ctx = (to: number) => {
        for (; p < to; p++) lines.push({ kind: 'ctx', text: base[p]!, n: p + 1 + offsets[i]! })
      }
      // Edits are sorted and each lies inside exactly one region.
      while (next[i]! < edits.length && edits[next[i]!]!.start <= e) {
        const ed = edits[next[i]!]!
        ctx(ed.start)
        for (let k = ed.start; k < ed.end; k++) lines.push({ kind: 'del', text: base[k]!, n: k + 1 })
        const at = ed.start + 1 + offsets[i]!
        ed.lines.forEach((t, k) => lines.push({ kind: 'add', text: t, n: at + k }))
        offsets[i] = offsets[i]! + ed.lines.length - (ed.end - ed.start)
        p = ed.end
        touched = true
        next[i]!++
      }
      ctx(e)
      out.push(lines)
      changed.push(touched)
    })
    blocks.push({ kind: 'change', start: s, end: e, cols: out, changed })
    pos = e
  }
  if (pos < base.length) blocks.push({ kind: 'same', start: pos, end: base.length, offsets: [...offsets] })
  return blocks
}
