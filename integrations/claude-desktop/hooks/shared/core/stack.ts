// A window's stack the way Graphite draws it: the newest branch on top, the
// trunk at the bottom, every branch above its parent. Shared by every surface
// (web, VS Code/Cursor, the Claude Code pane), so they order and summarize a
// stack the same way.

import type { WorkspaceInfo, WsBranch } from '../api/types'

export interface StackRow {
  branch: WsBranch
  /** Column: 0 for the main line, one more for each side branch. */
  lane: number
}

/**
 * Top-to-bottom rows of a window's stack. Each branch comes right below the
 * branches stacked on it; a branch's first child stays in its column and
 * later children (a fork in the stack) move one column right.
 */
export function stackRows(w: Pick<WorkspaceInfo, 'branches'>): StackRow[] {
  const byName = new Map(w.branches.map((b) => [b.name, b]))
  const out: StackRow[] = []
  const seen = new Set<string>()
  const visit = (b: WsBranch, lane: number) => {
    if (seen.has(b.name)) return
    seen.add(b.name)
    const kids = b.children.map((c) => byName.get(c)).filter((c): c is WsBranch => !!c)
    for (let i = kids.length - 1; i >= 0; i--) visit(kids[i]!, lane + i)
    out.push({ branch: b, lane })
  }
  // Roots: branches whose parent is not in this stack (the trunk, another window, a plain branch).
  const roots = w.branches.filter((b) => b.parent_kind !== 'stack' || !byName.has(b.parent))
  for (let i = roots.length - 1; i >= 0; i--) visit(roots[i]!, i)
  return out
}

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
