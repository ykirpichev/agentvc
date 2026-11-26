// The Stack view's data: `agentvc --json stack log --all` for this window's
// worktree, turned into a tree. No vscode imports, so it is unit tested.

export type ParentKind = 'trunk' | 'stack' | 'other_window' | 'branch'

export interface StackBranch {
  name: string
  parent: string
  parent_kind: ParentKind
  parent_window?: string
  parent_rev: string
  tip: string | null
  commits: number
  needs_restack: boolean
  current: boolean
  depth: number
  children: string[]
  pr?: number
  checked_out_elsewhere?: string
}

export interface OtherStack {
  path: string
  branches: { name: string; parent: string; tip: string | null; pr?: number }[]
}

export interface StackLog {
  window: string
  git_dir: string
  trunk: string
  base_ref: string
  base: string | null
  current: string | null
  branches: StackBranch[]
  pending?: { branch: string; onto: string }
  others?: OtherStack[]
}

/** One row of the tree: this window's branch, with its children nested. */
export interface StackNode {
  branch: StackBranch
  children: StackNode[]
}

/** Nest branches under their parents; roots sit on trunk or another window's branch. */
export function stackForest(log: StackLog): StackNode[] {
  const byName = new Map<string, StackNode>()
  for (const b of log.branches) byName.set(b.name, { branch: b, children: [] })
  const roots: StackNode[] = []
  for (const b of log.branches) {
    const node = byName.get(b.name)!
    const parent = b.parent_kind === 'stack' ? byName.get(b.parent) : undefined
    if (parent) parent.children.push(node)
    else roots.push(node)
  }
  return roots
}

/** The tree row's description: "2 commits · #12 · needs restack". */
export function branchDescription(b: StackBranch, log: Pick<StackLog, 'base_ref'>): string {
  const parts = [`${b.commits} commit${b.commits === 1 ? '' : 's'}`]
  if (b.pr) parts.push(`#${b.pr}`)
  if (b.needs_restack) parts.push('needs restack')
  if (b.tip === null) parts.push('branch deleted')
  if (b.depth === 0) {
    if (b.parent_kind === 'other_window') parts.push(`on ${b.parent} (other window)`)
    else if (b.parent_kind === 'branch') parts.push(`on ${b.parent}`)
    else parts.push(`on ${log.base_ref}`)
  }
  if (b.checked_out_elsewhere) parts.push(`open in ${b.checked_out_elsewhere}`)
  return parts.join(' · ')
}

/** Where the current branch sits: position in its chain, e.g. 2 of 5. */
export function position(log: StackLog): { branch: string; index: number; total: number } | undefined {
  const cur = log.branches.find((b) => b.current)
  if (!cur) return undefined
  const byName = new Map(log.branches.map((b) => [b.name, b]))
  let index = 1
  let at = cur
  while (at.parent_kind === 'stack' && byName.has(at.parent)) {
    at = byName.get(at.parent)!
    index++
  }
  let total = index
  let down = cur
  while (down.children.length === 1 && byName.has(down.children[0])) {
    down = byName.get(down.children[0])!
    total++
  }
  return { branch: cur.name, index, total }
}

/** Branches that need a restack, anywhere in this window. */
export function staleCount(log: StackLog): number {
  return log.branches.filter((b) => b.needs_restack).length
}

/** Whether `onto` is a valid drop target for `branch` (not itself or above it). */
export function canMoveOnto(log: StackLog, branch: string, onto: string): boolean {
  if (branch === onto) return false
  const byName = new Map(log.branches.map((b) => [b.name, b]))
  const up = new Set<string>()
  const todo = [branch]
  while (todo.length) {
    const n = todo.pop()!
    if (up.has(n)) continue
    up.add(n)
    for (const c of byName.get(n)?.children ?? []) todo.push(c)
  }
  return !up.has(onto)
}

/** `owner/name` from a GitHub remote URL. */
export function githubRepo(url: string): string | undefined {
  const m = url.trim().match(/github\.com[:/]([^/]+)\/([^/]+?)(?:\.git)?\/?$/)
  return m ? `${m[1]}/${m[2]}` : undefined
}
