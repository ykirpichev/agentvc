// What the pane draws, built from agentvcd's JSON API with the review UI's own
// layout and words (hooks/shared, copied from ui/src by sync-shared.sh), so the
// pane shows the same tree as the web, VS Code and Cursor. No `$` here: the
// hooks module hands in a fetch, so this file is plain logic.

import type {
  AgentvcBoard,
  AgentvcHunk,
  AgentvcPreview,
  AgentvcSel,
  AgentvcUi,
} from '../types'
import type {
  ConflictHunk,
  Graph,
  GraphAttempt,
  LiveAttempt,
  SpaceInfo,
  TaskSummary,
  WorkspaceInfo,
  WorkspaceList,
  WsBranch,
} from './shared/api/types'
import {
  canMoveBranch,
  canMoveOnto,
  canStackOnto,
  layout,
  stackStale,
  type Cell,
  type GraphRow,
} from './shared/core/graph'
import { shortSha, statusWords, theirsName, versionName } from './shared/core/words'
import { timeAgo } from './shared/lib/format'

export const EMPTY_BOARD: AgentvcBoard = {
  isReachable: false,
  error: null,
  space: null,
  graph: null,
  tasks: [],
  workspaces: [],
  here: null,
  running: [],
  fetchedAt: 0,
}

export const EMPTY_UI: AgentvcUi = {
  sel: null,
  pick: null,
  preview: null,
  showClosed: false,
  discarding: false,
  busy: false,
}

export type GetJson = (path: string) => Promise<unknown>

const norm = (p: string) => p.replace(/\\/g, '/').replace(/\/+$/, '')

/** The window holding `cwd`: the deepest workspace whose folder contains it. */
export function windowOf(list: readonly WorkspaceInfo[], cwd: string): string | null {
  const f = norm(cwd)
  const hit = list
    .filter(w => w.kind !== 'remote' && (norm(w.path) === f || f.startsWith(norm(w.path) + '/')))
    .sort((a, b) => b.path.length - a.path.length)[0]
  return hit?.id ?? null
}

/** Reads the whole board; throws when the daemon does not answer. */
export async function loadBoard(get: GetJson, cwd: string, nowS: number): Promise<AgentvcBoard> {
  const [space, graph, tasks, live] = await Promise.all([
    get('/api/space') as Promise<SpaceInfo>,
    get('/api/graph') as Promise<Graph>,
    get('/api/tasks') as Promise<TaskSummary[]>,
    get('/api/live') as Promise<LiveAttempt[]>,
  ])
  // Older daemons and spaces outside git have no workspaces: versions only.
  const ws = await (get('/api/workspaces') as Promise<WorkspaceList>).catch(() => null)
  const workspaces = ws?.workspaces ?? []
  return {
    isReachable: true,
    error: null,
    space: space.name,
    graph,
    tasks,
    workspaces,
    here: windowOf(workspaces, cwd) ?? ws?.here ?? null,
    running: live.filter(l => l.run?.alive).map(l => l.attempt.id),
    fetchedAt: nowS,
  }
}

// ---- the tree --------------------------------------------------------------

const CELL: Record<Cell, string> = {
  blank: '   ',
  pipe: '│  ',
  tee: '├─ ',
  elbow: '└─ ',
  trunk: '│  ',
}

export const gutter = (cells: readonly Cell[]) => cells.map(c => CELL[c]).join('')

export type Badge = { text: string; color: string }

/** One line of the tree, ready to draw. */
export type PaneRow = {
  key: string
  /** Box glyphs leading the row. */
  indent: string
  glyph: string
  glyphColor: string
  label: string
  isBold: boolean
  isStruck: boolean
  isDim: boolean
  sub: string | null
  badges: Badge[]
  /** What pressing the row selects; null for rows that only show. */
  press: AgentvcSel | null
  isSelected: boolean
  /** In pick mode: the picked row can move here. */
  isTarget: boolean
  /** The drop this target stands for. */
  drop: Drop | null
}

/** A move the person can make by pressing a target in pick mode. */
export type Drop =
  | { kind: 'onto-commit'; attempt: number; commit: string; where: string }
  | { kind: 'onto-version'; attempt: number; target: number; where: string }
  | { kind: 'branch-onto'; workspace: string; branch: string; onto: string; where: string }

const TONE: Record<string, string> = {
  ok: 'success',
  bad: 'error',
  run: 'claude',
  win: 'success',
  mute: 'inactive',
}

const sameSel = (a: AgentvcSel | null, b: AgentvcSel | null) =>
  !!a &&
  !!b &&
  (a.kind === 'version' && b.kind === 'version'
    ? a.id === b.id
    : a.kind === 'branch' && b.kind === 'branch' && a.workspace === b.workspace && a.name === b.name)

const blankRow = (key: string, indent: string): PaneRow => ({
  key,
  indent,
  glyph: '',
  glyphColor: 'inactive',
  label: '',
  isBold: false,
  isStruck: false,
  isDim: false,
  sub: null,
  badges: [],
  press: null,
  isSelected: false,
  isTarget: false,
  drop: null,
})

export function findVersion(board: AgentvcBoard, id: number): GraphAttempt | undefined {
  return board.graph?.attempts.find(g => g.attempt.id === id)
}

export function findBranch(
  board: AgentvcBoard,
  workspace: string,
  name: string,
): { w: WorkspaceInfo; b: WsBranch } | undefined {
  const w = board.workspaces.find(x => x.id === workspace)
  const b = w?.branches.find(x => x.name === name)
  return w && b ? { w, b } : undefined
}

/** The name the tree and the actions use for a version. */
export function nameOf(board: AgentvcBoard, id: number): string {
  const g = findVersion(board, id)
  return g ? versionName(g.attempt) : `a${id}`
}

const trunkName = (w: WorkspaceInfo) => w.trunk.replace(/^origin\//, '')

/** The tree: main, each window's stack, every version under what it is on. */
export function paneRows(board: AgentvcBoard, ui: AgentvcUi): PaneRow[] {
  const graph = board.graph
  if (!graph) return []
  const windows = board.workspaces
  const here = board.here
  const named = windows.filter(w => w.branches.length > 0 || w.pending).length > 1
  const tips = new Set(windows.flatMap(w => w.branches.map(b => b.tip).filter((t): t is string => !!t)))
  const { rows, onTip } = layout(graph, board.tasks, {
    showClosed: ui.showClosed,
    // This window first.
    windows: [...windows]
      .sort((a, b) => Number(b.id === here) - Number(a.id === here))
      .map(w => ({ id: w.id, base: w.base })),
    tips,
  })
  const byId = new Map(graph.attempts.map(g => [g.attempt.id, g]))
  const taskName = new Map(board.tasks.map(t => [t.id, t.goal]))
  const running = new Set(board.running)
  const newest = graph.trunk[0]?.commit

  const pick = ui.pick
  const pickedV = pick?.kind === 'version' ? byId.get(pick.id) : undefined
  const pickedB = pick?.kind === 'branch' ? findBranch(board, pick.workspace, pick.name) : undefined

  const out: PaneRow[] = []
  const draw = (r: GraphRow, lead: string) => {
    if (r.kind === 'trunk') {
      const c = graph.trunk.find(t => t.commit === r.commit)
      if (!c) return
      let drop: Drop | null = null
      if (pickedV && canMoveOnto(pickedV, c.commit)) {
        drop = { kind: 'onto-commit', attempt: pickedV.attempt.id, commit: c.commit, where: c.summary || shortSha(c.commit) }
      } else if (pickedB && c.commit === newest && canMoveBranch(pickedB.w, pickedB.b.name, pickedB.w.trunk)) {
        drop = { kind: 'branch-onto', workspace: pickedB.w.id, branch: pickedB.b.name, onto: pickedB.w.trunk, where: trunkName(pickedB.w) }
      }
      out.push({
        ...blankRow(r.key, lead),
        glyph: r.first ? '◉' : '●',
        glyphColor: c.on_upstream ? 'claude' : 'inactive',
        label: c.summary || shortSha(c.commit),
        sub: [
          r.first && c.on_upstream ? graph.upstream : null,
          c.on_upstream ? null : 'not on main',
          shortSha(c.commit),
          timeAgo(c.time),
        ]
          .filter(Boolean)
          .join(' · '),
        isTarget: !!drop,
        drop,
      })
      return
    }
    if (r.kind === 'task') {
      out.push({
        ...blankRow(r.key, lead + gutter(r.prefix)),
        label: r.task === null ? 'No task' : (taskName.get(r.task) ?? `Task ${r.task}`),
        isBold: true,
      })
      return
    }
    if (r.kind === 'version') {
      const g = byId.get(r.id)
      if (!g) return
      const a = g.attempt
      const st = statusWords(a.status)
      const isRunning = running.has(a.id)
      const stale = stackStale(graph, g)
      const onBranch = !!g.base_commit && tips.has(g.base_commit)
      const badges: Badge[] = []
      if (g.conflicts > 0) badges.push({ text: `${g.conflicts} conflict${g.conflicts === 1 ? '' : 's'}`, color: 'error' })
      else if (stale) badges.push({ text: `${versionName(stale.attempt)} changed`, color: 'warning' })
      else if (!onBranch && !g.stacked_on && (g.behind ?? 0) > 0) badges.push({ text: `↓${g.behind}`, color: 'warning' })
      if (g.pr) badges.push({ text: `#${g.pr.number}`, color: 'suggestion' })
      if (isRunning) badges.push({ text: 'running', color: 'claude' })
      const parentTask = g.stacked_on ? byId.get(g.stacked_on.attempt)?.attempt.task_id : undefined
      const sel: AgentvcSel = { kind: 'version', id: a.id }
      const drop: Drop | null =
        pickedV && canStackOnto(graph, pickedV, g)
          ? { kind: 'onto-version', attempt: pickedV.attempt.id, target: a.id, where: versionName(a) }
          : null
      out.push({
        ...blankRow(r.key, lead + gutter(r.prefix)),
        glyph: isRunning ? '●' : a.status === 'promoted' ? '★' : a.status === 'abandoned' ? '⊘' : '○',
        glyphColor: isRunning ? 'claude' : TONE[st.tone] ?? 'inactive',
        label: versionName(a),
        isBold: a.status === 'promoted',
        isStruck: a.status === 'abandoned',
        isDim: a.status === 'abandoned',
        sub: [
          st.label,
          g.stacked_on && parentTask !== a.task_id && a.task_id !== null
            ? (taskName.get(a.task_id) ?? `Task ${a.task_id}`)
            : null,
        ]
          .filter(Boolean)
          .join(' · '),
        badges,
        press: sel,
        isSelected: sameSel(sel, ui.sel) || sameSel(sel, ui.pick),
        isTarget: !!drop,
        drop,
      })
      return
    }
    // A window's stack of branches.
    const w = windows.find(x => x.id === r.workspace)
    if (!w) return
    const base = lead + (r.lead === 'trunk' ? '│  ' : '   ')
    const queued = w.requests.filter(q => q.status === 'queued')
    if (w.branches.length === 0 && !w.pending && queued.length === 0) return
    if (named || w.state === 'agent_running') {
      out.push({
        ...blankRow(`${r.key}-head`, base),
        label: named ? w.name : '',
        sub: named && w.id === here ? 'this window' : null,
        isBold: true,
        badges: [
          ...(w.state === 'agent_running' ? [{ text: 'agent working', color: 'claude' }] : []),
          ...(w.kind === 'remote' ? [{ text: `read-only · ${w.machine}`, color: 'inactive' }] : []),
        ],
      })
    }
    if (w.pending) {
      out.push({
        ...blankRow(`${r.key}-stop`, base),
        glyph: '!',
        glyphColor: 'error',
        label: `${w.pending.branch} clashes with its parent.`,
        press: { kind: 'branch', workspace: w.id, name: w.pending.branch },
        badges: [{ text: 'stopped', color: 'error' }],
      })
    }
    for (const b of w.branches) {
      const indent = base + '   '.repeat(Math.max(0, b.depth)) + '└ '
      const badges: Badge[] = []
      if (b.current) badges.push({ text: 'checked out', color: 'inactive' })
      if (b.conflicts > 0) badges.push({ text: `${b.conflicts} conflict${b.conflicts === 1 ? '' : 's'}`, color: 'error' })
      else if (b.needs_restack) badges.push({ text: 'needs restack', color: 'warning' })
      else if (b.behind > 0 && b.parent_kind === 'trunk') badges.push({ text: `↓${b.behind}`, color: 'warning' })
      if (b.pr) badges.push({ text: `#${b.pr.number}`, color: 'suggestion' })
      if (b.parent_kind === 'other_window') badges.push({ text: `on ${b.parent_workspace ?? 'another window'}`, color: 'inactive' })
      const sel: AgentvcSel = { kind: 'branch', workspace: w.id, name: b.name }
      const drop: Drop | null =
        pickedB && pickedB.w.id === w.id && canMoveBranch(w, pickedB.b.name, b.name)
          ? { kind: 'branch-onto', workspace: w.id, branch: pickedB.b.name, onto: b.name, where: b.name }
          : null
      const dot = b.conflicts > 0 ? 'error' : b.status === 'running' ? 'claude' : b.pr?.state === 'merged' ? 'success' : b.needs_restack ? 'inactive' : 'success'
      out.push({
        ...blankRow(`${r.key}-b-${b.name}`, indent),
        glyph: '●',
        glyphColor: dot,
        label: b.name,
        isBold: b.current,
        badges,
        press: sel,
        isSelected: sameSel(sel, ui.sel) || sameSel(sel, ui.pick),
        isTarget: !!drop,
        drop,
      })
      const under = b.tip ? onTip.get(b.tip) : undefined
      for (const sub of under ?? []) draw(sub, base + '   '.repeat(Math.max(0, b.depth) + 1))
    }
    for (const q of queued) {
      out.push({
        ...blankRow(`${r.key}-q-${q.id}`, base),
        label: `${q.op} ${q.branch ?? ''}${q.onto ? ` onto ${q.onto}` : ''}`,
        badges: [{ text: 'queued', color: 'claude' }],
      })
    }
  }
  for (const r of rows) draw(r, '')
  return out
}

/** Versions the tree leaves out until "Show N not chosen". */
export const closedCount = (board: AgentvcBoard) =>
  board.graph?.attempts.filter(g => g.attempt.status === 'abandoned').length ?? 0

// ---- moves -----------------------------------------------------------------

type RawPreview = {
  rebase: {
    changes: { path: string }[]
    conflict_files?: { path: string; remaining: number }[]
    conflicts?: string[]
    rebased: boolean
  }
}

/** What a rebase preview says, in the pane's words. */
export function toPreview(
  raw: RawPreview,
  attempt: number,
  onto: string,
  where: string,
  flags: { isStack: boolean; isUpdate: boolean },
): AgentvcPreview {
  const clash = (raw.rebase.conflict_files ?? []).filter(f => f.remaining > 0).map(f => f.path)
  const clean = raw.rebase.changes.map(c => c.path).filter(p => !clash.includes(p))
  return { attempt, onto, where, ...flags, clean, clash, error: null }
}

export function previewTitle(board: AgentvcBoard, p: AgentvcPreview): string {
  const name = nameOf(board, p.attempt)
  if (p.isUpdate) return `Update ${name} from ${p.where}?`
  if (p.isStack) return `Stack ${name} on ${p.where}?`
  return `Rebase ${name} onto ${p.where}?`
}

// ---- conflicts -------------------------------------------------------------

export function toHunks(hunks: readonly ConflictHunk[]): AgentvcHunk[] {
  return hunks.map(h => ({ index: h.index, ours: h.ours, theirs: h.theirs, other: theirsName(h) }))
}

// ---- diffs -----------------------------------------------------------------

/** Lines the diff compares at most; past that a file opens in the browser. */
export const MAX_DIFF_LINES = 4000
const CONTEXT = 3

const splitLines = (s: string) => {
  if (s === '') return []
  const lines = s.split('\n')
  if (lines[lines.length - 1] === '') lines.pop()
  return lines
}

/**
 * Unified hunks (`@@ -a,b +c,d @@`) between two texts, for `Code format="diff"`.
 * A plain longest-common-subsequence diff: files here are small, and past
 * MAX_DIFF_LINES it answers null.
 */
export function unifiedDiff(before: string, after: string): string | null {
  const a = splitLines(before)
  const b = splitLines(after)
  // Trim the common head and tail, so the table covers only what changed.
  let head = 0
  while (head < a.length && head < b.length && a[head] === b[head]) head++
  let tail = 0
  while (tail < a.length - head && tail < b.length - head && a[a.length - 1 - tail] === b[b.length - 1 - tail]) tail++
  const am = a.slice(head, a.length - tail)
  const bm = b.slice(head, b.length - tail)
  if (am.length * bm.length > MAX_DIFF_LINES * MAX_DIFF_LINES / 4) return null
  if (am.length === 0 && bm.length === 0) return ''
  const n = am.length
  const m = bm.length
  const lcs: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0))
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i]![j] = am[i] === bm[j] ? lcs[i + 1]![j + 1]! + 1 : Math.max(lcs[i + 1]![j]!, lcs[i]![j + 1]!)
    }
  }
  type Op = { t: ' ' | '-' | '+'; s: string }
  const ops: Op[] = a.slice(0, head).map(s => ({ t: ' ' as const, s }))
  let i = 0
  let j = 0
  while (i < n || j < m) {
    if (i < n && j < m && am[i] === bm[j]) {
      ops.push({ t: ' ', s: am[i]! })
      i++
      j++
    } else if (i < n && (j === m || lcs[i + 1]![j]! >= lcs[i]![j + 1]!)) {
      ops.push({ t: '-', s: am[i]! })
      i++
    } else {
      ops.push({ t: '+', s: bm[j]! })
      j++
    }
  }
  for (const s of a.slice(a.length - tail)) ops.push({ t: ' ', s })

  // Group changes with CONTEXT lines around them into hunks.
  const out: string[] = []
  let k = 0
  let oldLine = 1
  let newLine = 1
  const lineAt: { o: number; n: number }[] = []
  for (const op of ops) {
    lineAt.push({ o: oldLine, n: newLine })
    if (op.t !== '+') oldLine++
    if (op.t !== '-') newLine++
  }
  while (k < ops.length) {
    if (ops[k]!.t === ' ') {
      k++
      continue
    }
    const start = Math.max(0, k - CONTEXT)
    // Take in later changes while the gap to them is at most 2*CONTEXT lines.
    let lastChange = k
    for (let x = k; x < ops.length; x++) {
      if (ops[x]!.t !== ' ') lastChange = x
      else if (x - lastChange > CONTEXT * 2) break
    }
    const end = Math.min(ops.length - 1, lastChange + CONTEXT)
    const slice = ops.slice(start, end + 1)
    const oldCount = slice.filter(o => o.t !== '+').length
    const newCount = slice.filter(o => o.t !== '-').length
    const at = lineAt[start]!
    out.push(`@@ -${oldCount ? at.o : at.o - 1},${oldCount} +${newCount ? at.n : at.n - 1},${newCount} @@`)
    for (const o of slice) out.push(o.t + o.s)
    k = end + 1
  }
  return out.join('\n')
}
