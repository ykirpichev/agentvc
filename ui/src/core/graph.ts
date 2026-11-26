// Lay out the branch graph: main's commits down the left (the trunk), each
// task's versions hung off the commit they are based on, forks nested under
// the version they came from. Pure, so every surface draws the same rows.

import type { Graph, GraphAttempt, TaskSummary } from '../api/types'

/** One cell of the tree's left gutter. */
export type Cell = 'blank' | 'pipe' | 'tee' | 'elbow' | 'trunk'

export type GraphRow =
  | { kind: 'trunk'; key: string; commit: string; first: boolean; last: boolean }
  /** A window's stack of branches, drawn on the commit its trunk is at. */
  | { kind: 'window'; key: string; workspace: string; commit: string; lead: Cell }
  | { kind: 'task'; key: string; task: number | null; commit: string; prefix: Cell[] }
  | { kind: 'version'; key: string; id: number; prefix: Cell[] }

export interface LayoutOptions {
  /** Show versions that were not chosen (abandoned). */
  showClosed?: boolean
  /** Each window's stack and the main commit it sits on. */
  windows?: { id: string; base: string | null }[]
  /** Tips of stacked branches: versions based on one hang under that branch. */
  tips?: ReadonlySet<string>
}

export interface Layout {
  rows: GraphRow[]
  /** Rows for the versions based on each branch tip, gutters from the branch. */
  onTip: Map<string, GraphRow[]>
}

const isRoot = (g: GraphAttempt) => g.attempt.parent_attempt === null && g.attempt.task_id === null

/** Versions the graph shows. */
export function visibleVersions(graph: Graph, opts: LayoutOptions = {}): GraphAttempt[] {
  return graph.attempts.filter(
    (g) => !isRoot(g) && (opts.showClosed || g.attempt.status !== 'abandoned'),
  )
}

/**
 * One tree: main's commits that something sits on (and its newest), each
 * window's stack on the commit it is based on, and every version under the
 * commit, branch or version it is based on.
 */
export function layout(graph: Graph, tasks: TaskSummary[], opts: LayoutOptions = {}): Layout {
  const shown = visibleVersions(graph, opts)
  const byId = new Map(shown.map((g) => [g.attempt.id, g]))
  // Nest a fork under its parent while both sit on the same commit; a fork
  // rebased elsewhere hangs off its own base instead.
  const parentOf = (g: GraphAttempt): number | null => {
    // A version stacked onto another sits under it.
    if (g.stacked_on) return byId.has(g.stacked_on.attempt) ? g.stacked_on.attempt : null
    const p = g.attempt.parent_attempt
    const pg = p === null ? undefined : byId.get(p)
    return pg && pg.base_commit === g.base_commit ? p : null
  }
  const kids = new Map<number, GraphAttempt[]>()
  const roots: GraphAttempt[] = []
  for (const g of [...shown].sort((a, b) => a.attempt.id - b.attempt.id)) {
    const p = parentOf(g)
    if (p === null) roots.push(g)
    else kids.set(p, [...(kids.get(p) ?? []), g])
  }

  const tips = opts.tips ?? new Set<string>()
  const onBranch = (g: GraphAttempt) => !!g.base_commit && tips.has(g.base_commit)
  const all = graph.trunk.filter((c) => c.on_upstream || !tips.has(c.commit))
  const onTrunk = new Set(all.map((c) => c.commit))
  const fallback = all[all.length - 1]?.commit ?? ''
  const anchor = (g: GraphAttempt) => (g.base_commit && onTrunk.has(g.base_commit) ? g.base_commit : fallback)
  const windowAt = (w: { base: string | null }) => (w.base && onTrunk.has(w.base) ? w.base : (all[0]?.commit ?? ''))
  const taskOrder = new Map(tasks.map((t, i) => [t.id, i]))

  const walk = (out: GraphRow[], list: GraphAttempt[], prefix: Cell[]) =>
    list.forEach((g, i) => {
      const last = i === list.length - 1
      out.push({ kind: 'version', key: `v${g.attempt.id}`, id: g.attempt.id, prefix: [...prefix, last ? 'elbow' : 'tee'] })
      walk(out, kids.get(g.attempt.id) ?? [], [...prefix, last ? 'blank' : 'pipe'])
    })
  /** Task groups for `here`, each with its versions, under `lead`. */
  const hang = (out: GraphRow[], here: GraphAttempt[], commit: string, lead: Cell[]) => {
    const taskIds = [...new Set(here.map((g) => g.attempt.task_id))].sort(
      (a, b) => (taskOrder.get(a ?? -1) ?? 1e9) - (taskOrder.get(b ?? -1) ?? 1e9),
    )
    taskIds.forEach((t, j) => {
      const lastT = j === taskIds.length - 1
      out.push({ kind: 'task', key: `t${t}@${commit}`, task: t, commit, prefix: [...lead, lastT ? 'elbow' : 'tee'] })
      walk(
        out,
        here.filter((g) => g.attempt.task_id === t),
        [...lead, lastT ? 'blank' : 'pipe'],
      )
    })
  }

  const onTip = new Map<string, GraphRow[]>()
  for (const tip of tips) {
    const here = roots.filter((g) => g.base_commit === tip)
    if (!here.length) continue
    const out: GraphRow[] = []
    hang(out, here, tip, [])
    onTip.set(tip, out)
  }

  const trunkRoots = roots.filter((g) => !onBranch(g))
  const windows = opts.windows ?? []
  // Main's commits with nothing on them are left out, but for the newest.
  const trunk = all.filter(
    (c, i) =>
      i === 0 || trunkRoots.some((g) => anchor(g) === c.commit) || windows.some((w) => windowAt(w) === c.commit),
  )
  const rows: GraphRow[] = []
  trunk.forEach((c, i) => {
    const lastTrunk = i === trunk.length - 1
    rows.push({ kind: 'trunk', key: `c${c.commit}`, commit: c.commit, first: i === 0, last: lastTrunk })
    const lead: Cell = lastTrunk ? 'blank' : 'trunk'
    for (const w of windows.filter((x) => windowAt(x) === c.commit)) {
      rows.push({ kind: 'window', key: `w${w.id}`, workspace: w.id, commit: c.commit, lead })
    }
    hang(
      rows,
      trunkRoots.filter((g) => anchor(g) === c.commit),
      c.commit,
      [lead],
    )
  })
  return { rows, onTip }
}

/** The versions `g` is stacked on, nearest first. */
export function stackedChain(graph: Graph, g: GraphAttempt): GraphAttempt[] {
  const out: GraphAttempt[] = []
  let cur = g.stacked_on
  while (cur && out.length < 64) {
    const p = graph.attempts.find((x) => x.attempt.id === cur!.attempt)
    if (!p) break
    out.push(p)
    cur = p.stacked_on
  }
  return out
}

/** `g` is stacked on a version that has moved on since. */
export function stackStale(graph: Graph, g: GraphAttempt): GraphAttempt | undefined {
  if (!g.stacked_on) return undefined
  const p = graph.attempts.find((x) => x.attempt.id === g.stacked_on!.attempt)
  return p && p.head_snapshot !== null && p.head_snapshot !== g.stacked_on.snapshot ? p : undefined
}

/** `src` can be stacked on top of version `target`. */
export function canStackOnto(graph: Graph, src: GraphAttempt | undefined, target: GraphAttempt): boolean {
  if (!src || src.attempt.id === 1 || target.attempt.id === 1 || src.attempt.id === target.attempt.id) return false
  const movable = (s: string) => s === 'exploring' || s === 'passed' || s === 'failed' || s === 'promoted'
  if (!movable(src.attempt.status) || !movable(target.attempt.status) || target.conflicts > 0) return false
  // Already on target's latest state.
  if (src.stacked_on?.attempt === target.attempt.id && src.stacked_on.snapshot === target.head_snapshot) return false
  // Target sits on src (directly or further down): that would make a loop.
  return !stackedChain(graph, target).some((x) => x.attempt.id === src.attempt.id)
}

/** `src` can be moved onto `target` (a trunk commit). */
export function canMoveOnto(g: GraphAttempt | undefined, commit: string): boolean {
  if (!g || g.attempt.id === 1) return false
  const s = g.attempt.status
  return (s === 'exploring' || s === 'passed' || s === 'failed' || s === 'promoted') && g.base_commit !== commit
}
