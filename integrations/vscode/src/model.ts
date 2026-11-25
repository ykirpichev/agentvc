// Pure helpers that shape daemon data for the editor. No `vscode` imports,
// so they are unit-tested in plain Node.

import * as path from 'node:path'
import type { Attempt, AttemptStats, FileChange, LiveAttempt, Ref, Snapshot, Status, StatusCounts } from './types'

/** Attempts of one task as a forest: children are nested under the attempt they forked from. */
export interface AttemptNode {
  attempt: Attempt
  children: AttemptNode[]
}

export function attemptForest(attempts: Attempt[]): AttemptNode[] {
  const byId = new Map<number, AttemptNode>()
  for (const a of [...attempts].sort((x, y) => x.id - y.id)) byId.set(a.id, { attempt: a, children: [] })
  const roots: AttemptNode[] = []
  for (const node of byId.values()) {
    const parent = node.attempt.parent_attempt
    const p = parent === null ? undefined : byId.get(parent)
    if (p) p.children.push(node)
    else roots.push(node)
  }
  return roots
}

/** "2 passed · 1 exploring", in the order a reviewer cares about. */
export function countsLabel(c: StatusCounts): string {
  const order: Status[] = ['promoted', 'passed', 'exploring', 'failed', 'abandoned']
  const parts = order.filter((s) => c[s] > 0).map((s) => `${c[s]} ${s}`)
  return parts.length ? parts.join(' · ') : 'no attempts'
}

export function isOpen(status: Status): boolean {
  return status !== 'abandoned' && status !== 'promoted'
}

/** Short relative time: "now", "5m", "3h", "2d". */
export function ago(unixSeconds: number, now = Date.now() / 1000): string {
  const s = Math.max(0, Math.round(now - unixSeconds))
  if (s < 45) return 'now'
  if (s < 3600) return `${Math.round(s / 60)}m`
  if (s < 86400) return `${Math.round(s / 3600)}h`
  return `${Math.round(s / 86400)}d`
}

/** One line under an attempt: status, check, snapshots, cost. */
export function attemptDescription(a: Attempt, stats?: AttemptStats, live?: LiveAttempt): string {
  const parts: string[] = []
  if (live?.run?.alive) parts.push('running')
  else parts.push(a.status)
  if (stats?.check) parts.push(stats.check.passed ? 'check ✓' : 'check ✗')
  if (stats) parts.push(`${stats.snapshots} snap${stats.snapshots === 1 ? '' : 's'}`)
  if (live && live.files_changed) parts.push(`${live.files_changed} file${live.files_changed === 1 ? '' : 's'}`)
  if (stats?.cost_usd != null) parts.push(`$${stats.cost_usd.toFixed(2)}`)
  if (stats) parts.push(ago(stats.last_activity))
  else parts.push(ago(a.created_at))
  return parts.join(' · ')
}

export function snapshotLabel(s: Snapshot): string {
  return `s${s.id} ${s.trigger}`
}

/** The ref a snapshot's changes are shown against: the previous snapshot, else the fork point. */
export function snapshotBase(s: Snapshot, all: Snapshot[], attempt: Attempt): Ref | undefined {
  const earlier = all.filter((x) => x.attempt_id === s.attempt_id && x.id < s.id)
  if (earlier.length) return `s${Math.max(...earlier.map((x) => x.id))}`
  return attempt.parent_snapshot === null ? undefined : `s${attempt.parent_snapshot}`
}

/** The attempt whose view contains `dir`, if any (longest match wins). */
export function attemptForPath(dir: string, attempts: Attempt[]): Attempt | undefined {
  const norm = (p: string) => path.resolve(p).replace(/[\\/]+$/, '')
  const d = norm(dir)
  let best: Attempt | undefined
  for (const a of attempts) {
    const v = norm(a.view_path)
    if ((d === v || d.startsWith(v + path.sep)) && (!best || v.length > norm(best.view_path).length)) best = a
  }
  return best
}

export interface DiffEntry {
  path: string
  kind: FileChange['kind']
  /** Whether each side has the file. */
  left: boolean
  right: boolean
}

/** Sides of a native diff for each change: added files are empty on the left, removed on the right. */
export function diffEntries(changes: FileChange[]): DiffEntry[] {
  return [...changes]
    .sort((x, y) => x.path.localeCompare(y.path))
    .map((c) => ({
      path: c.path,
      kind: c.kind,
      left: c.kind !== 'added',
      right: c.kind !== 'removed',
    }))
}

export function kindLetter(kind: FileChange['kind']): string {
  return kind === 'added' ? 'A' : kind === 'removed' ? 'D' : 'M'
}

/** Parses "a3", "s12", "3" (attempt) into a ref. */
export function parseRef(s: string): Ref | undefined {
  const m = /^\s*([as]?)(\d+)\s*$/.exec(s)
  if (!m) return undefined
  return `${(m[1] || 'a') as 'a' | 's'}${Number(m[2])}` as Ref
}
