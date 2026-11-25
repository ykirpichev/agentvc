import type { Attempt } from '../api/types'

export interface TreeRow {
  attempt: Attempt
  depth: number
  /** True when this node's parent is outside the list (e.g. the workdir a1). */
  root: boolean
  last: boolean
}

/** Flatten attempts into a depth-first tree ordered by creation. */
export function attemptTree(attempts: Attempt[]): TreeRow[] {
  const ids = new Set(attempts.map((a) => a.id))
  const children = new Map<number, Attempt[]>()
  const roots: Attempt[] = []
  for (const a of [...attempts].sort((x, y) => x.id - y.id)) {
    if (a.parent_attempt !== null && ids.has(a.parent_attempt)) {
      children.set(a.parent_attempt, [...(children.get(a.parent_attempt) ?? []), a])
    } else {
      roots.push(a)
    }
  }
  const out: TreeRow[] = []
  const walk = (a: Attempt, depth: number, last: boolean, root: boolean) => {
    out.push({ attempt: a, depth, last, root })
    const kids = children.get(a.id) ?? []
    kids.forEach((k, i) => walk(k, depth + 1, i === kids.length - 1, false))
  }
  roots.forEach((r, i) => walk(r, 0, i === roots.length - 1, true))
  return out
}

export interface VisibleRow extends TreeRow {
  /** Descendants hidden because this abandoned node is folded. */
  folded: number
  /** On the path from a root to a promoted attempt. */
  onWinningPath: boolean
}

/**
 * The tree as shown: subtrees under abandoned attempts fold away unless
 * their id is in `expanded`, and the path to the winner is marked.
 */
export function visibleTree(attempts: Attempt[], expanded: ReadonlySet<number>): VisibleRow[] {
  const rows = attemptTree(attempts)
  const byId = new Map(attempts.map((a) => [a.id, a]))
  const path = new Set<number>()
  for (const a of attempts) {
    if (a.status !== 'promoted') continue
    let cur: Attempt | undefined = a
    while (cur && !path.has(cur.id)) {
      path.add(cur.id)
      cur = cur.parent_attempt === null ? undefined : byId.get(cur.parent_attempt)
    }
  }
  const out: VisibleRow[] = []
  for (let i = 0; i < rows.length; i++) {
    const r = rows[i]!
    let end = i + 1
    while (end < rows.length && rows[end]!.depth > r.depth) end++
    const descendants = end - i - 1
    const fold = r.attempt.status === 'abandoned' && descendants > 0 && !expanded.has(r.attempt.id)
    out.push({ ...r, folded: fold ? descendants : 0, onWinningPath: path.has(r.attempt.id) })
    if (fold) i = end - 1
  }
  return out
}
