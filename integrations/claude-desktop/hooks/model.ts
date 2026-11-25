// What the panel knows about a space, built from agentvcd's JSON API. No `$`
// here: the hooks module hands in a fetch, so this file is plain logic.

import type {
  AgentvcAttempt,
  AgentvcBoard,
  AgentvcLive,
  AgentvcStats,
  AgentvcStatus,
  AgentvcTask,
} from '../types'

/** An agent that snapshotted this recently counts as active without a run. */
export const ACTIVE_WINDOW_S = 5 * 60

export const EMPTY_BOARD: AgentvcBoard = {
  isReachable: false,
  error: null,
  space: null,
  tasks: [],
  attempts: [],
  live: [],
  inbox: [],
  stats: {},
  fetchedAt: 0,
}

export type GetJson = (path: string) => Promise<unknown>

type RawLive = {
  attempt: { id: number; task_id: number | null; model: string | null }
  task_goal: string | null
  head: { trigger: string } | null
  stats: { last_activity: number }
  files_changed: number
  overlaps: Record<string, number[]>
  run: { alive: boolean; log_tail: string[] } | null
}

type RawStats = {
  attempt: number
  cost_usd: number | null
  summary: string | null
  check: { passed: boolean } | null
}

export function toLive(raw: RawLive): AgentvcLive {
  const tail = raw.run?.log_tail ?? []
  return {
    attempt: raw.attempt.id,
    task: raw.attempt.task_id,
    goal: raw.task_goal,
    model: raw.attempt.model,
    step: raw.head?.trigger ?? null,
    files: raw.files_changed,
    overlaps: Object.keys(raw.overlaps ?? {}).length,
    isRunning: raw.run?.alive === true,
    lastActivity: raw.stats.last_activity,
    logTail: tail[tail.length - 1] ?? null,
  }
}

/** Running under `agentvc run`, or snapshotted lately (a hooked agent). */
export function isActive(live: AgentvcLive, nowS: number): boolean {
  return live.isRunning || nowS - live.lastActivity <= ACTIVE_WINDOW_S
}

/** A task is settled once it has a winner, or nothing is left open or passed. */
export function isSettled(task: AgentvcTask): boolean {
  const a = task.attempts
  return a.promoted > 0 || (a.total > 0 && a.exploring === 0 && a.passed === 0)
}

/** Reads the whole board; throws when the daemon does not answer. */
export async function loadBoard(
  get: GetJson,
  expanded: readonly number[],
  nowS: number,
): Promise<AgentvcBoard> {
  const [space, tasks, attempts, live, inbox] = await Promise.all([
    get('/api/space') as Promise<{ name: string }>,
    get('/api/tasks') as Promise<AgentvcTask[]>,
    get('/api/attempts') as Promise<AgentvcAttempt[]>,
    get('/api/live') as Promise<RawLive[]>,
    get('/api/inbox') as Promise<{ task: { id: number } }[]>,
  ])
  const known = new Set(tasks.map(t => t.id))
  const statLists = await Promise.all(
    expanded
      .filter(id => known.has(id))
      .map(id => get(`/api/tasks/${id}/stats`) as Promise<RawStats[]>),
  )
  const stats: Record<string, AgentvcStats> = {}
  for (const list of statLists) {
    for (const s of list) {
      stats[`a${s.attempt}`] = {
        cost: s.cost_usd,
        summary: s.summary,
        check: s.check ? s.check.passed : null,
      }
    }
  }
  return {
    isReachable: true,
    error: null,
    space: space.name,
    tasks: [...tasks].sort((a, b) => b.updated_at - a.updated_at),
    attempts: attempts.map(a => ({
      id: a.id,
      task_id: a.task_id,
      parent_attempt: a.parent_attempt,
      actor: a.actor,
      model: a.model,
      prompt: a.prompt,
      status: a.status,
      reason: a.reason,
      created_at: a.created_at,
    })),
    live: live.map(toLive),
    inbox: inbox.map(i => i.task.id),
    stats,
    fetchedAt: nowS,
  }
}

/** `prefix` leads the attempt's row; `pad` leads a line drawn under it. */
export type TreeRow = { attempt: AgentvcAttempt; prefix: string; pad: string }

/**
 * A task's attempts as a tree, drawn with box glyphs. An attempt whose parent
 * is outside the task (the root workdir, another task) starts a branch.
 */
export function taskTree(attempts: readonly AgentvcAttempt[], task: number): TreeRow[] {
  const mine = attempts.filter(a => a.task_id === task).sort((a, b) => a.id - b.id)
  const ids = new Set(mine.map(a => a.id))
  const children = new Map<number | null, AgentvcAttempt[]>()
  for (const a of mine) {
    const parent = a.parent_attempt !== null && ids.has(a.parent_attempt) ? a.parent_attempt : null
    children.set(parent, [...(children.get(parent) ?? []), a])
  }
  const rows: TreeRow[] = []
  const walk = (parent: number | null, indent: string) => {
    const kids = children.get(parent) ?? []
    kids.forEach((a, i) => {
      const last = i === kids.length - 1
      const below = indent + (last ? '   ' : '│  ')
      const hasKids = (children.get(a.id) ?? []).length > 0
      rows.push({ attempt: a, prefix: indent + (last ? '└─ ' : '├─ '), pad: below + (hasKids ? '│ ' : '  ') })
      walk(a.id, below)
    })
  }
  walk(null, '')
  return rows
}

export type StatusLook = { glyph: string; label: string; color: string; isDim: boolean }

export function statusLook(status: AgentvcStatus, isRunning: boolean): StatusLook {
  switch (status) {
    case 'promoted':
      return { glyph: '★', label: 'winner', color: 'success', isDim: false }
    case 'passed':
      return { glyph: '✓', label: 'passed', color: 'success', isDim: false }
    case 'failed':
      return { glyph: '✗', label: 'failed', color: 'error', isDim: false }
    case 'abandoned':
      return { glyph: '⊘', label: 'abandoned', color: 'inactive', isDim: true }
    default:
      return isRunning
        ? { glyph: '●', label: 'running', color: 'claude', isDim: false }
        : { glyph: '○', label: 'exploring', color: 'suggestion', isDim: false }
  }
}

/** `42s`, `3m`, `5h`, `2d`. */
export function ago(seconds: number): string {
  const s = Math.max(0, Math.round(seconds))
  if (s < 60) return `${s}s`
  if (s < 3600) return `${Math.floor(s / 60)}m`
  if (s < 86400) return `${Math.floor(s / 3600)}h`
  return `${Math.floor(s / 86400)}d`
}

export function money(usd: number | null | undefined): string | null {
  return typeof usd === 'number' ? `$${usd.toFixed(2)}` : null
}

/** One line under a task's title: its attempts by outcome. */
export function taskSummary(task: AgentvcTask, running: number): string {
  const a = task.attempts
  const parts = [`${a.total} attempt${a.total === 1 ? '' : 's'}`]
  if (running > 0) parts.push(`${running} running`)
  if (a.passed > 0) parts.push(`${a.passed} passed`)
  if (a.failed > 0) parts.push(`${a.failed} failed`)
  if (a.abandoned > 0) parts.push(`${a.abandoned} abandoned`)
  return parts.join(' · ')
}
