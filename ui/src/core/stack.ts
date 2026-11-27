// A window's stack the way Graphite draws it: the newest branch on top, the
// trunk at the bottom, every branch above its parent. Shared by every surface
// (web, VS Code/Cursor, the Claude Code pane), so they order and summarize a
// stack the same way.

import type { WorkspaceInfo, WsBranch } from '../api/types'

export interface StackRow {
  branch: WsBranch
  /** Column of the branch's dot: 0 for the first line, one more for each side stack open beside it. */
  lane: number
}

/** A line from a branch's dot down to its parent's dot (`to` = rows.length is the trunk). */
export interface StackEdge {
  from: number
  to: number
  fromLane: number
  toLane: number
}

export interface StackGraph {
  rows: StackRow[]
  edges: StackEdge[]
  /** Columns in use, at least 1. */
  lanes: number
}

const TRUNK = '\0trunk'

/**
 * Top-to-bottom rows of a window's stack, laid out like `gt log`: each branch
 * comes right below the branches stacked on it, the longest stack is drawn
 * first and keeps the first column, a straight stack stays in one
 * column, and a side stack (a fork, or a second stack on main) gets the next
 * free column and joins its parent's dot. The trunk is one row below the last.
 */
export function stackGraph(w: Pick<WorkspaceInfo, 'branches'>): StackGraph {
  const byName = new Map(w.branches.map((b) => [b.name, b]))
  const order: WsBranch[] = []
  const seen = new Set<string>()
  const kidsOf = (b: WsBranch) => b.children.map((c) => byName.get(c)).filter((c): c is WsBranch => !!c)
  const sizes = new Map<string, number>()
  const size = (b: WsBranch, path = new Set<string>()): number => {
    const known = sizes.get(b.name)
    if (known !== undefined) return known
    if (path.has(b.name)) return 0
    path.add(b.name)
    const n = 1 + kidsOf(b).reduce((t, k) => t + size(k, path), 0)
    sizes.set(b.name, n)
    return n
  }
  // The longest stack is drawn first, so it keeps the first column.
  const longestFirst = (bs: WsBranch[]) => [...bs].sort((a, b) => size(b) - size(a))
  const visit = (b: WsBranch) => {
    if (seen.has(b.name)) return
    seen.add(b.name)
    for (const k of longestFirst(kidsOf(b))) visit(k)
    order.push(b)
  }
  // Roots: branches whose parent is not in this stack (the trunk, another window, a plain branch).
  const roots = w.branches.filter((b) => b.parent_kind !== 'stack' || !byName.has(b.parent))
  for (const r of longestFirst(roots)) visit(r)

  // Each open column waits for the dot it runs down to.
  const open: ({ waitFor: string; from: number } | null)[] = []
  const rows: StackRow[] = []
  const edges: StackEdge[] = []
  const place = (key: string, at: number, forced?: number): number => {
    const waiting = open.flatMap((o, i) => (o?.waitFor === key ? [i] : []))
    let lane = forced ?? waiting[0] ?? open.indexOf(null)
    if (lane < 0) lane = open.length
    for (const i of waiting) {
      edges.push({ from: open[i]!.from, to: at, fromLane: i, toLane: lane })
      open[i] = null
    }
    return lane
  }
  order.forEach((b, at) => {
    const lane = place(b.name, at)
    rows.push({ branch: b, lane })
    open[lane] = { waitFor: b.parent_kind === 'stack' && byName.has(b.parent) ? b.parent : TRUNK, from: at }
  })
  place(TRUNK, order.length, 0)
  const lanes = Math.max(1, ...rows.map((r) => r.lane + 1))
  return { rows, edges, lanes }
}

/** The rows of `stackGraph`, for surfaces that only indent. */
export const stackRows = (w: Pick<WorkspaceInfo, 'branches'>): StackRow[] => stackGraph(w).rows

/** How many commits main has that the stack's bottom branches lack. */
export function mainAhead(w: Pick<WorkspaceInfo, 'branches'>): number {
  return Math.max(0, ...w.branches.filter((b) => b.parent_kind === 'trunk').map((b) => b.behind))
}

/** Sync would change something: main moved, a parent moved, or a PR merged. */
export function needsSync(w: Pick<WorkspaceInfo, 'branches'>): boolean {
  return mainAhead(w) > 0 || w.branches.some((b) => b.needs_restack || b.pr?.state === 'merged')
}

/** One short phrase for a window in the "other windows" list. */
export function windowState(w: WorkspaceInfo): { label: string; tone: 'bad' | 'run' | 'warn' | 'mute' } | null {
  if (w.pending || w.branches.some((b) => b.conflicts > 0)) return { label: 'stopped on a clash', tone: 'bad' }
  if (w.state === 'agent_running') return { label: 'agent working', tone: 'run' }
  if (needsSync(w)) return { label: 'needs sync', tone: 'warn' }
  return null
}

/** The window's name without the remote prefix on its trunk, e.g. `main`. */
export const trunkName = (w: Pick<WorkspaceInfo, 'trunk'>) => w.trunk.replace(/^origin\//, '')
