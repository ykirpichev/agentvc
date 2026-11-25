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
