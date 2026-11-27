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
import { canMoveBranch, type Cell } from './shared/core/graph'
import { nextStep } from './shared/core/next'
import { mainAhead, needsSync, stackRows, trunkName, windowState } from './shared/core/stack'
import { prWords, statusWords, theirsName, tryName, versionName } from './shared/core/words'

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
    space: space.project?.name ?? space.name,
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

/** The window the pane shows: the one Claude Code runs in, else the first with a stack. */
export function shownWindow(board: AgentvcBoard): WorkspaceInfo | undefined {
  const ws = board.workspaces
  return ws.find(w => w.id === board.here) ?? ws.find(w => w.branches.length > 0 || !!w.pending) ?? ws[0]
}

/** Tasks whose tries are listed under Agent work: open, and not made into a branch. */
export function agentTasks(board: AgentvcBoard): { task: TaskSummary; tries: GraphAttempt[] }[] {
  const graph = board.graph
  if (!graph) return []
  const onBranch = new Set(board.workspaces.flatMap(w => w.branches.map(b => b.task).filter((t): t is number => t !== null)))
  return board.tasks
    .filter(t => !onBranch.has(t.id))
    .map(task => ({ task, tries: graph.attempts.filter(g => g.attempt.task_id === task.id) }))
    .filter(({ tries }) => tries.length > 0 && nextStep(tries).kind !== 'done')
}

/**
 * The tree, the same as the web sidebar: this window's stack like `gt log`
 * (newest branch on top, main at the bottom), the other windows one line
 * each, then agent work: each open task with its tries.
 */
export function paneRows(board: AgentvcBoard, ui: AgentvcUi): PaneRow[] {
  const graph = board.graph
  if (!graph) return []
  const out: PaneRow[] = []
  const w = shownWindow(board)
  const pickedB = ui.pick?.kind === 'branch' ? findBranch(board, ui.pick.workspace, ui.pick.name) : undefined
  const triesOf = new Map(board.tasks.map(t => [t.id, t.attempts.total - t.attempts.abandoned]))

  if (w && (w.branches.length > 0 || w.pending)) {
    out.push({
      ...blankRow(`w${w.id}-head`, ''),
      label: w.name,
      sub: w.id === board.here ? 'this window' : null,
      isBold: true,
      badges: [
        ...(w.state === 'agent_running' ? [{ text: 'agent working', color: 'claude' }] : []),
        ...(w.kind === 'remote' ? [{ text: `read-only · ${w.machine}`, color: 'inactive' }] : []),
      ],
    })
    if (w.pending) {
      out.push({
        ...blankRow(`w${w.id}-stop`, ''),
        glyph: '!',
        glyphColor: 'error',
        label: `Stopped at ${w.pending.branch}: a file changed on both sides`,
        press: { kind: 'branch', workspace: w.id, name: w.pending.branch },
      })
    } else if (needsSync(w)) {
      const ahead = mainAhead(w)
      out.push({
        ...blankRow(`w${w.id}-sync`, ''),
        glyph: '↓',
        glyphColor: 'warning',
        label: ahead > 0 ? `${trunkName(w)} moved ${ahead} commit${ahead === 1 ? '' : 's'}. Sync to rebase your stack on it.` : 'Some branches are behind their parent. Sync to rebase them.',
      })
    }
    for (const { branch: b, lane } of stackRows(w)) {
      const badges: Badge[] = []
      if (b.conflicts > 0) badges.push({ text: 'clash', color: 'error' })
      if (b.status === 'running') badges.push({ text: 'agent', color: 'claude' })
      const tries = b.task !== null ? (triesOf.get(b.task) ?? 0) : 0
      if (tries > 1) badges.push({ text: `${tries} tries`, color: 'suggestion' })
      if (b.parent_kind === 'other_window') badges.push({ text: `on ${b.parent_workspace ?? 'another window'}`, color: 'inactive' })
      if (b.pr) {
        const pw = prWords(b.pr.state, b.pr.status)
        badges.push({ text: `#${b.pr.number}${pw ? ` ${pw.short}` : ''}`, color: pw ? (TONE[pw.tone] ?? 'suggestion') : 'suggestion' })
      }
      const sel: AgentvcSel = { kind: 'branch', workspace: w.id, name: b.name }
      const drop: Drop | null =
        pickedB && pickedB.w.id === w.id && canMoveBranch(w, pickedB.b.name, b.name)
          ? { kind: 'branch-onto', workspace: w.id, branch: pickedB.b.name, onto: b.name, where: b.name }
          : null
      out.push({
        ...blankRow(`w${w.id}-b-${b.name}`, '  '.repeat(lane)),
        glyph: b.current ? '●' : '○',
        glyphColor: b.conflicts > 0 ? 'error' : b.current ? 'claude' : 'inactive',
        label: b.name,
        isBold: b.current,
        sub: b.current ? "you're here" : null,
        badges,
        press: sel,
        isSelected: sameSel(sel, ui.sel) || sameSel(sel, ui.pick),
        isTarget: !!drop,
        drop,
      })
    }
    const toTrunk: Drop | null =
      pickedB && pickedB.w.id === w.id && canMoveBranch(w, pickedB.b.name, w.trunk)
        ? { kind: 'branch-onto', workspace: w.id, branch: pickedB.b.name, onto: w.trunk, where: trunkName(w) }
        : null
    out.push({ ...blankRow(`w${w.id}-trunk`, ''), glyph: '■', glyphColor: 'inactive', label: trunkName(w), isDim: true, isTarget: !!toTrunk, drop: toTrunk })
    for (const q of w.requests.filter(r => r.status === 'queued')) {
      out.push({
        ...blankRow(`w${w.id}-q-${q.id}`, ''),
        label: `${q.op} ${q.branch ?? ''}${q.onto ? ` onto ${q.onto}` : ''}`,
        badges: [{ text: 'waiting for the agent', color: 'claude' }],
      })
    }
  }

  const others = board.workspaces.filter(x => x !== w && (x.branches.length > 0 || x.pending || x.state === 'agent_running'))
  if (others.length > 0) {
    out.push({ ...blankRow('others-head', ''), label: 'OTHER WINDOWS', isDim: true })
    for (const x of others) {
      const st = windowState(x)
      const top = stackRows(x)[0]?.branch
      out.push({
        ...blankRow(`win-${x.id}`, ''),
        label: x.name,
        sub: x.branches.length ? `${x.branches.length} branch${x.branches.length === 1 ? '' : 'es'}` : 'no stack',
        badges: st ? [{ text: st.label, color: st.tone === 'bad' ? 'error' : st.tone === 'run' ? 'claude' : st.tone === 'warn' ? 'warning' : 'inactive' }] : [],
        press: top ? { kind: 'branch', workspace: x.id, name: top.name } : null,
      })
    }
  }

  const work = agentTasks(board)
  if (work.length > 0) {
    out.push({ ...blankRow('work-head', ''), label: 'AGENT WORK', isDim: true })
    const running = new Set(board.running)
    for (const { task, tries } of work) {
      out.push({ ...blankRow(`t${task.id}`, ''), label: task.goal, isBold: true })
      for (const g of tries) {
        const a = g.attempt
        if (a.status === 'abandoned' && !ui.showClosed) continue
        const st = statusWords(a.status)
        const isRunning = running.has(a.id)
        const badges: Badge[] = []
        if (g.conflicts > 0) badges.push({ text: `${g.conflicts} clash${g.conflicts === 1 ? '' : 'es'}`, color: 'error' })
        if (g.pr) badges.push({ text: `#${g.pr.number}`, color: 'suggestion' })
        if (isRunning) badges.push({ text: 'running', color: 'claude' })
        const sel: AgentvcSel = { kind: 'version', id: a.id }
        out.push({
          ...blankRow(`v${a.id}`, '  '),
          glyph: isRunning ? '●' : a.status === 'promoted' ? '★' : a.status === 'abandoned' ? '⊘' : '○',
          glyphColor: isRunning ? 'claude' : (TONE[st.tone] ?? 'inactive'),
          label: tryName(a, g.agent?.notes.at(-1)?.text ?? a.prompt),
          isBold: a.status === 'promoted',
          isStruck: a.status === 'abandoned',
          isDim: a.status === 'abandoned',
          sub: `${versionName(a)} · ${st.label}`,
          badges,
          press: sel,
          isSelected: sameSel(sel, ui.sel),
        })
      }
    }
  }
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
