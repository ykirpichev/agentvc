// Lay out the branch graph: main's commits down the left (the trunk), each
// task's versions hung off the commit they are based on, forks nested under
// the version they came from. Pure, so every surface draws the same rows.

import type { Graph, GraphAttempt, TaskSummary } from '../api/types'

/** One cell of the tree's left gutter. */
export type Cell = 'blank' | 'pipe' | 'tee' | 'elbow' | 'trunk'

export type GraphRow =
  | { kind: 'trunk'; key: string; commit: string; first: boolean; last: boolean }
  | { kind: 'task'; key: string; task: number | null; commit: string; prefix: Cell[] }
  | { kind: 'version'; key: string; id: number; prefix: Cell[] }

export interface LayoutOptions {
  /** Show versions that were not chosen (abandoned). */
  showClosed?: boolean
}

const isRoot = (g: GraphAttempt) => g.attempt.parent_attempt === null && g.attempt.task_id === null

/** Versions the graph shows. */
export function visibleVersions(graph: Graph, opts: LayoutOptions = {}): GraphAttempt[] {
  return graph.attempts.filter(
    (g) => !isRoot(g) && (opts.showClosed || g.attempt.status !== 'abandoned'),
  )
}

export function layout(graph: Graph, tasks: TaskSummary[], opts: LayoutOptions = {}): GraphRow[] {
  const shown = visibleVersions(graph, opts)
  const byId = new Map(shown.map((g) => [g.attempt.id, g]))
  // Nest a fork under its parent while both sit on the same commit; a fork
  // rebased elsewhere hangs off its own base instead.
  const parentOf = (g: GraphAttempt): number | null => {
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

  const trunk = graph.trunk
  const onTrunk = new Set(trunk.map((c) => c.commit))
  const fallback = trunk[trunk.length - 1]?.commit ?? ''
  const anchor = (g: GraphAttempt) => (g.base_commit && onTrunk.has(g.base_commit) ? g.base_commit : fallback)
  const taskOrder = new Map(tasks.map((t, i) => [t.id, i]))

  const rows: GraphRow[] = []
  const walk = (list: GraphAttempt[], prefix: Cell[]) =>
    list.forEach((g, i) => {
      const last = i === list.length - 1
      rows.push({ kind: 'version', key: `v${g.attempt.id}`, id: g.attempt.id, prefix: [...prefix, last ? 'elbow' : 'tee'] })
      walk(kids.get(g.attempt.id) ?? [], [...prefix, last ? 'blank' : 'pipe'])
    })

  trunk.forEach((c, i) => {
    const lastTrunk = i === trunk.length - 1
    rows.push({ kind: 'trunk', key: `c${c.commit}`, commit: c.commit, first: i === 0, last: lastTrunk })
    const here = roots.filter((g) => anchor(g) === c.commit)
    const taskIds = [...new Set(here.map((g) => g.attempt.task_id))].sort(
      (a, b) => (taskOrder.get(a ?? -1) ?? 1e9) - (taskOrder.get(b ?? -1) ?? 1e9),
    )
    const lead: Cell = lastTrunk ? 'blank' : 'trunk'
    taskIds.forEach((t, j) => {
      const lastT = j === taskIds.length - 1
      rows.push({ kind: 'task', key: `t${t}@${c.commit}`, task: t, commit: c.commit, prefix: [lead, lastT ? 'elbow' : 'tee'] })
      walk(
        here.filter((g) => g.attempt.task_id === t),
        [lead, lastT ? 'blank' : 'pipe'],
      )
    })
  })
  return rows
}

/** `src` can be moved onto `target` (a trunk commit). */
export function canMoveOnto(g: GraphAttempt | undefined, commit: string): boolean {
  if (!g || g.attempt.id === 1) return false
  const s = g.attempt.status
  return (s === 'exploring' || s === 'passed' || s === 'failed' || s === 'promoted') && g.base_commit !== commit
}
