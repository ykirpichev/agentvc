// agentvc panel: the review UI's branch tree in a pane beside the chat. main,
// each window's stack of branches, and every agent version under the commit,
// branch or version it is based on; press a row for its actions (each with a
// line saying what it does), its conflicts and its diff. It is the same tree,
// words and rules as the web, VS Code and Cursor UI (hooks/shared is copied from
// ui/src). It talks to agentvcd through Claude Code (`$.http.fetch`, never a
// browser), so the daemon's localhost guard holds.

import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { AgentvcBoard, AgentvcDetail, AgentvcFileDiff, AgentvcSel, AgentvcUi } from '../types'
import {
  EMPTY_BOARD,
  EMPTY_UI,
  closedCount,
  findBranch,
  findVersion,
  loadBoard,
  nameOf,
  paneRows,
  previewTitle,
  toHunks,
  toPreview,
  unifiedDiff,
  type Drop,
  type PaneRow,
} from './model'
import type { ConflictHunk, GraphAttempt, HunkTake, WorkspaceInfo, WsOp } from './shared/api/types'
import { stackStale } from './shared/core/graph'
import { HELP, WORDS, shortSha, statusWords, versionName } from './shared/core/words'

const PANE = 'agentvc'
const TITLE = 'agentvc'
/** Files whose diff the pane draws; the rest open in the browser. */
const MAX_FILES = 6

// Change a shape tag when that value's fields change: it declines what an
// earlier version stored, which outlives a reload.
const boardAtom = atom({ plugin: 'agentvc-panel', key: 'board' } as const, EMPTY_BOARD, { shape: 'board-2' })
const uiAtom = atom({ plugin: 'agentvc-panel', key: 'ui' } as const, EMPTY_UI, { shape: 'ui-1' })
const EMPTY_DETAIL: AgentvcDetail = {
  key: '',
  isLoading: false,
  error: null,
  files: [],
  moreFiles: 0,
  commits: [],
  conflicts: [],
}
const detailAtom = atom({ plugin: 'agentvc-panel', key: 'detail' } as const, EMPTY_DETAIL, { shape: 'detail-1' })

type $ = EngineInterface

// Read from the options at each load; module variables start over on a
// reload, while the board itself lives in $.state.
let base = 'http://127.0.0.1:7420'
let pollMs = 2000
let autoOpen = true
let inFlight = false
let lastJson = ''
let hasOpened = false
let cwd = '/'

/** Links open in the browser; surfaces make links only of https and localhost. */
const webBase = () => base.replace('//127.0.0.1', '//localhost')
const nowS = () => Math.floor(Date.now() / 1000)
const enc = encodeURIComponent

function errorMessage(text: string, status: number): string {
  try {
    return JSON.parse(text)?.error?.message ?? `HTTP ${status}`
  } catch {
    return `HTTP ${status}`
  }
}

async function getJson($: $, path: string): Promise<unknown> {
  const res = await $.http.fetch(base + path, { headers: { accept: 'application/json' } })
  if (!res.ok) throw new Error(errorMessage(res.text, res.status))
  return JSON.parse(res.text) as unknown
}

async function postJson($: $, path: string, body: unknown = {}): Promise<unknown> {
  const res = await $.http.fetch(base + path, {
    method: 'POST',
    headers: { accept: 'application/json', 'content-type': 'application/json' },
    body: JSON.stringify(body),
  })
  if (!res.ok) throw new Error(errorMessage(res.text, res.status))
  return res.text ? (JSON.parse(res.text) as unknown) : null
}

// ---- polling ----------------------------------------------------------------

/** What the selected row's details depend on: reload when it changes. */
function detailKey(board: AgentvcBoard, sel: AgentvcSel | null): string {
  if (!sel) return ''
  if (sel.kind === 'version') {
    const g = findVersion(board, sel.id)
    return g ? `v${g.attempt.id}:${g.head_snapshot}:${g.conflicts}:${g.base_commit}` : ''
  }
  const hit = findBranch(board, sel.workspace, sel.name)
  if (!hit) return ''
  return `b${hit.w.id}/${hit.b.name}:${hit.b.tip}:${hit.b.parent_rev}:${hit.w.pending?.branch === hit.b.name}:${hit.b.conflicts}`
}

async function poll($: $): Promise<void> {
  if (inFlight) return
  inFlight = true
  try {
    let board: AgentvcBoard
    try {
      board = await loadBoard(path => getJson($, path), cwd, nowS())
    } catch (err) {
      const prev = await read($, boardAtom)
      board = { ...prev, isReachable: false, error: err instanceof Error ? err.message : String(err), fetchedAt: nowS() }
    }
    const json = JSON.stringify({ ...board, fetchedAt: 0 })
    if (json !== lastJson) {
      lastJson = json
      await update($, boardAtom, () => board)
      $.ui.status(board.isReachable && board.running.length > 0 ? `agentvc: ${board.running.length} running` : undefined)
    }
    if (board.isReachable) {
      const ui = await read($, uiAtom)
      // A selection whose row is gone (landed, deleted) is dropped.
      if (ui.sel && !detailKey(board, ui.sel)) await update($, uiAtom, u => ({ ...u, sel: null, discarding: false }))
      else if ((await read($, detailAtom)).key !== detailKey(board, ui.sel)) await loadDetail($)
    }
    if (board.isReachable && autoOpen && !hasOpened) {
      hasOpened = true
      void $.ui.open({ id: PANE, title: TITLE })
    }
  } finally {
    inFlight = false
  }
}

// ---- details ----------------------------------------------------------------

type RawFile = { content: string | null; binary: boolean; too_large: boolean }

async function fileText($: $, ref: string, path: string): Promise<RawFile | null> {
  try {
    return (await getJson($, `/api/file?ref=${enc(ref)}&path=${enc(path)}`)) as RawFile
  } catch {
    return null // added or removed on this side
  }
}

async function fileDiff($: $, from: string, to: string, path: string, kind: string): Promise<AgentvcFileDiff> {
  const [a, b] = await Promise.all([
    kind === 'added' ? null : fileText($, from, path),
    kind === 'removed' || kind === 'deleted' ? null : fileText($, to, path),
  ])
  if (a?.binary || b?.binary) return { path, kind, diff: null, note: 'Binary file' }
  if (a?.too_large || b?.too_large) return { path, kind, diff: null, note: 'Too large to show here' }
  const diff = unifiedDiff(a?.content ?? '', b?.content ?? '')
  if (diff === null) return { path, kind, diff: null, note: 'Too large to show here' }
  return { path, kind, diff: diff || null, note: diff ? null : 'Only the mode changed' }
}

async function loadDetail($: $): Promise<void> {
  const board = await read($, boardAtom)
  const { sel } = await read($, uiAtom)
  const key = detailKey(board, sel)
  if (!sel || !key) {
    await update($, detailAtom, () => EMPTY_DETAIL)
    return
  }
  await update($, detailAtom, d => ({ ...(d.key.split(':')[0] === key.split(':')[0] ? d : EMPTY_DETAIL), key, isLoading: true }))
  let next: AgentvcDetail = { ...EMPTY_DETAIL, key }
  try {
    if (sel.kind === 'version') {
      const g = findVersion(board, sel.id)!
      const diff = (await getJson($, `/api/diff?b=a${g.attempt.id}`)) as {
        a_ref: string
        b_ref: string
        changes: { path: string; kind: string }[]
      }
      const files = await Promise.all(
        diff.changes.slice(0, MAX_FILES).map(c => fileDiff($, diff.a_ref, diff.b_ref, c.path, c.kind)),
      )
      let conflicts: AgentvcDetail['conflicts'] = []
      if (g.conflicts > 0) {
        const c = (await getJson($, `/api/attempts/${g.attempt.id}/conflicts`)) as {
          files: { path: string; hunks: ConflictHunk[] }[]
        }
        conflicts = c.files.map(f => ({ path: f.path, hunks: toHunks(f.hunks) }))
      }
      next = { ...next, files, moreFiles: Math.max(0, diff.changes.length - MAX_FILES), conflicts }
    } else {
      const { w, b } = findBranch(board, sel.workspace, sel.name)!
      const detail = (await getJson($, `/api/workspaces/${enc(w.id)}/branches/${b.name.split('/').map(enc).join('/')}`)) as {
        base: string
        tip: string
        commits: { commit: string; subject: string }[]
        files: { path: string; kind: string }[]
      }
      const files = await Promise.all(
        detail.files.slice(0, MAX_FILES).map(f => fileDiff($, `git:${detail.base}`, `git:${detail.tip}`, f.path, f.kind)),
      )
      let conflicts: AgentvcDetail['conflicts'] = []
      if (w.pending?.branch === b.name) {
        const c = (await getJson($, `/api/workspaces/${enc(w.id)}/conflicts`)) as {
          files: { path: string; hunks: ConflictHunk[] }[]
        }
        conflicts = c.files.map(f => ({ path: f.path, hunks: toHunks(f.hunks) }))
      }
      next = {
        ...next,
        files,
        moreFiles: Math.max(0, detail.files.length - MAX_FILES),
        commits: detail.commits.map(c => ({ commit: c.commit, subject: c.subject })),
        conflicts,
      }
    }
  } catch (err) {
    next = { ...next, error: err instanceof Error ? err.message : String(err) }
  }
  // A newer selection won while this loaded: keep its details.
  if (detailKey(await read($, boardAtom), (await read($, uiAtom)).sel) === key) await update($, detailAtom, () => next)
}

// ---- actions ----------------------------------------------------------------

/** Run an action: say what happened, or why it failed, then refresh. */
async function act($: $, done: string, fn: () => Promise<unknown>): Promise<void> {
  await update($, uiAtom, u => ({ ...u, busy: true }))
  try {
    await fn()
    if (done) $.ui.toast(done)
  } catch (err) {
    $.ui.toast(err instanceof Error ? err.message : String(err))
  } finally {
    await update($, uiAtom, u => ({ ...u, busy: false }))
    lastJson = ''
    await poll($)
    await loadDetail($)
  }
}

async function select($: $, sel: AgentvcSel): Promise<void> {
  const ui = await read($, uiAtom)
  const same = JSON.stringify(ui.sel) === JSON.stringify(sel)
  await update($, uiAtom, u => ({ ...u, sel: same ? null : sel, pick: null, preview: null, discarding: false }))
  await loadDetail($)
}

async function startPick($: $, sel: AgentvcSel): Promise<void> {
  await update($, uiAtom, u => ({ ...u, sel, pick: sel, preview: null, discarding: false }))
}

async function cancelPick($: $): Promise<void> {
  await update($, uiAtom, u => ({ ...u, pick: null, preview: null }))
}

/** Ask the daemon what a rebase would do, and show it before anything moves. */
async function preview($: $, attempt: number, onto: string | undefined, where: string, flags: { isStack: boolean; isUpdate: boolean }) {
  await update($, uiAtom, u => ({ ...u, busy: true }))
  try {
    const raw = (await postJson($, `/api/attempts/${attempt}/rebase`, { onto, preview: true })) as Parameters<typeof toPreview>[0]
    const p = toPreview(raw, attempt, onto ?? '', where, flags)
    await update($, uiAtom, u => ({ ...u, pick: null, preview: p, busy: false }))
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err)
    await update($, uiAtom, u => ({
      ...u,
      pick: null,
      busy: false,
      preview: { attempt, onto: onto ?? '', where, ...flags, clean: [], clash: [], error: msg },
    }))
  }
}

async function applyPreview($: $): Promise<void> {
  const { preview: p } = await read($, uiAtom)
  if (!p) return
  const board = await read($, boardAtom)
  const name = nameOf(board, p.attempt)
  await update($, uiAtom, (u): AgentvcUi => ({ ...u, preview: null, sel: { kind: 'version', id: p.attempt } }))
  await act($, p.clash.length ? `${name} has ${p.clash.length} clash${p.clash.length === 1 ? '' : 'es'} to settle below.` : `${p.isStack ? 'Stacked' : 'Rebased'} ${name} onto ${p.where}.`, () =>
    postJson($, `/api/attempts/${p.attempt}/rebase`, {
      ...(p.onto ? { onto: p.onto, no_fetch: true } : {}),
      keep_conflicts: p.clash.length > 0,
    }),
  )
}

async function drop($: $, d: Drop): Promise<void> {
  const board = await read($, boardAtom)
  if (d.kind === 'onto-commit') return preview($, d.attempt, d.commit, d.where, { isStack: false, isUpdate: false })
  if (d.kind === 'onto-version') return preview($, d.attempt, `a${d.target}`, d.where, { isStack: true, isUpdate: false })
  const w = board.workspaces.find(x => x.id === d.workspace)
  await update($, uiAtom, u => ({ ...u, pick: null }))
  if (w) await stackEdit($, w, { op: 'move', branch: d.branch, onto: d.onto }, `Moved ${d.branch} onto ${d.where}.`)
}

type WsOutcome = { outcome: 'applied' | 'queued'; reason?: string; result?: { conflict?: { branch: string } | null } }

/** Change a window's stack; a clash stops there and opens it here. */
async function stackEdit($: $, w: WorkspaceInfo, op: WsOp, done: string): Promise<void> {
  const keep = op.op === 'move' || op.op === 'restack'
  let said = done
  await act($, '', async () => {
    const r = (await postJson($, `/api/workspaces/${enc(w.id)}/stack`, keep ? { ...op, keep_conflicts: true } : op)) as WsOutcome
    const stopped = r.result?.conflict?.branch
    if (r.outcome === 'queued') said = `Queued for ${w.name}: ${r.reason ?? 'an agent is working there'}. It applies when the agent stops.`
    else if (stopped) {
      said = `${stopped} clashes with its parent. Pick what to keep below.`
      await update($, uiAtom, (u): AgentvcUi => ({ ...u, sel: { kind: 'branch', workspace: w.id, name: stopped } }))
    }
  })
  $.ui.toast(said)
}

async function resolveHunk($: $, sel: AgentvcSel, path: string, index: number, take: HunkTake): Promise<void> {
  const url =
    sel.kind === 'version'
      ? `/api/attempts/${sel.id}/conflicts/resolve`
      : `/api/workspaces/${enc(sel.workspace)}/conflicts/resolve`
  await act($, '', () => postJson($, url, { path, hunks: [{ index, take }] }))
}

async function finish($: $, w: WorkspaceInfo, abort: boolean): Promise<void> {
  let said = ''
  await act($, '', async () => {
    const r = (await postJson($, `/api/workspaces/${enc(w.id)}/${abort ? 'abort' : 'continue'}`)) as {
      outcome: string
      conflicts: { branch: string | null } | null
    }
    said =
      r.outcome === 'aborted'
        ? 'Stopped. The stack is back where it was.'
        : r.outcome === 'stopped'
          ? `${r.conflicts?.branch ?? 'The next branch'} clashes too. Pick what to keep below.`
          : 'Restack finished.'
    if (r.outcome === 'stopped' && r.conflicts?.branch) {
      const b = r.conflicts.branch
      await update($, uiAtom, (u): AgentvcUi => ({ ...u, sel: { kind: 'branch', workspace: w.id, name: b } }))
    }
  })
  if (said) $.ui.toast(said)
}

// ---- drawing ----------------------------------------------------------------

type El = ReturnType<EngineInterface['ui']['resolve']>

function Tree($: $, { el, rows, ui }: { el: El; rows: PaneRow[]; ui: AgentvcUi }) {
  const { Box, Text, Button } = el
  return (
    <Box flexDirection="column">
      {rows.map(r => (
        <Box key={r.key} flexDirection="row">
          <Text dimColor mono>
            {r.indent}
          </Text>
          {r.glyph ? <Text color={r.glyphColor}>{r.glyph} </Text> : ''}
          {r.press ? (
            <Button key={`row-${r.key}`} plain dimColor={r.isDim} onPress={() => select($, r.press!)}>
              <Text bold={r.isBold || r.isSelected} strikethrough={r.isStruck} color={r.isSelected ? 'claude' : undefined}>
                {r.isSelected ? '▸ ' : ''}
                {r.label}
              </Text>
              {r.sub ? <Text dimColor> {r.sub}</Text> : ''}
              {r.badges.map(b => (
                <Text key={b.text} color={b.color}>
                  {' '}
                  {b.text}
                </Text>
              ))}
            </Button>
          ) : (
            <Text wrap="end">
              <Text bold={r.isBold}>{r.label}</Text>
              {r.sub ? <Text dimColor> {r.sub}</Text> : ''}
              {r.badges.map(b => (
                <Text key={b.text} color={b.color}>
                  {' '}
                  {b.text}
                </Text>
              ))}
            </Text>
          )}
          {ui.pick && r.isTarget && r.drop ? (
            <Button key={`drop-${r.key}`} variant="primary" onPress={() => drop($, r.drop!)}>
              ← here
            </Button>
          ) : (
            ''
          )}
        </Box>
      ))}
    </Box>
  )
}

/** One action: its button, and a line under it saying what it does. */
function Action({ el, id, label, help, onPress, primary }: { el: El; id: string; label: string; help: string; onPress: () => unknown; primary?: boolean }) {
  const { Box, Text, Button } = el
  return (
    <Box key={`act-${id}`} flexDirection="column">
      <Button key={id} variant={primary ? 'primary' : 'secondary'} onPress={() => void onPress()}>
        {label}
      </Button>
      <Text dimColor size="caption" wrap="wrap">
        {help}
      </Text>
    </Box>
  )
}

function Conflicts($: $, { el, sel, detail, mine }: { el: El; sel: AgentvcSel; detail: AgentvcDetail; mine: string }) {
  const { Box, Text, Button, Code } = el
  if (detail.conflicts.length === 0) return null
  return (
    <Box flexDirection="column" gap={1}>
      <Text bold color="error">
        {WORDS.resolve}
      </Text>
      <Text dimColor size="caption">
        {HELP.resolve}
      </Text>
      {detail.conflicts.map(f => (
        <Box key={`cf-${f.path}`} flexDirection="column" gap={1}>
          <Text mono bold>
            {f.path}
          </Text>
          {f.hunks.length === 0 && (
            <Text dimColor size="caption" wrap="wrap">
              No conflict markers in this file (deleted on one side, or binary). Resolve it in your editor.
            </Text>
          )}
          {f.hunks.map(hk => (
            <Box key={`h-${f.path}-${hk.index}`} flexDirection="column">
              <Text dimColor size="caption">
                {hk.other}
              </Text>
              <Code source={hk.theirs || ' '} path={f.path} />
              <Text dimColor size="caption">
                {mine}
              </Text>
              <Code source={hk.ours || ' '} path={f.path} />
              <Box flexDirection="row" gap={1}>
                <Button key={`keep-theirs-${f.path}-${hk.index}`} onPress={() => resolveHunk($, sel, f.path, hk.index, 'theirs')}>
                  {hk.other === 'main' ? WORDS.keepMain : WORDS.keep(hk.other)}
                </Button>
                <Button key={`keep-ours-${f.path}-${hk.index}`} onPress={() => resolveHunk($, sel, f.path, hk.index, 'ours')}>
                  {WORDS.keep(mine)}
                </Button>
                <Button key={`keep-both-${f.path}-${hk.index}`} variant="primary" onPress={() => resolveHunk($, sel, f.path, hk.index, 'both')}>
                  {WORDS.keepBoth}
                </Button>
              </Box>
            </Box>
          ))}
        </Box>
      ))}
    </Box>
  )
}

function Files({ el, detail, more }: { el: El; detail: AgentvcDetail; more: string }) {
  const { Box, Text, Code, Link } = el
  if (detail.files.length === 0) return detail.isLoading ? <Text dimColor>Loading changes…</Text> : <Text dimColor>No changes.</Text>
  return (
    <Box flexDirection="column" gap={1}>
      <Text bold dimColor size="caption">
        CHANGES · {detail.files.length + detail.moreFiles} file{detail.files.length + detail.moreFiles === 1 ? '' : 's'}
      </Text>
      {detail.files.map(f => (
        <Box key={`f-${f.path}`} flexDirection="column">
          <Text mono bold>
            {f.path}
            <Text dimColor> {f.kind}</Text>
          </Text>
          {f.diff ? <Code source={f.diff} format="diff" /> : <Text dimColor size="caption">{f.note}</Text>}
        </Box>
      ))}
      {detail.moreFiles > 0 && <Link href={more}>{`${detail.moreFiles} more file${detail.moreFiles === 1 ? '' : 's'}: open in the browser`}</Link>}
    </Box>
  )
}

function VersionDetail($: $, { el, board, ui, detail, g }: { el: El; board: AgentvcBoard; ui: AgentvcUi; detail: AgentvcDetail; g: GraphAttempt }) {
  const { Box, Text, Button, Input, Link } = el
  const a = g.attempt
  const name = versionName(a)
  const graph = board.graph!
  const st = statusWords(a.status)
  const task = board.tasks.find(t => t.id === a.task_id)
  const stale = stackStale(graph, g)
  const parent = g.stacked_on ? findVersion(board, g.stacked_on.attempt) : undefined
  const tips = new Set(board.workspaces.flatMap(w => w.branches.map(b => b.tip)))
  const onBranch = !!g.base_commit && tips.has(g.base_commit)
  const movable = a.status !== 'abandoned' && a.id !== 1
  const open = a.status === 'exploring' || a.status === 'passed' || a.status === 'failed'
  const isRunning = board.running.includes(a.id)
  const behind = g.behind ?? 0
  const link = `${webBase()}/review/v/${a.id}`
  return (
    <Box flexDirection="column" gap={1}>
      <Box flexDirection="column">
        <Text bold>{name}</Text>
        {task && (
          <Text dimColor wrap="wrap">
            {task.goal}
          </Text>
        )}
        <Text dimColor size="caption">
          {st.label}
          {parent ? ` · stacked on ${versionName(parent.attempt)}` : g.base_commit ? ` · on ${shortSha(g.base_commit)}` : ''}
          {g.pr ? ` · PR #${g.pr.number}${g.pr.state && g.pr.state !== 'open' ? ` (${g.pr.state})` : ''}` : ''}
        </Text>
      </Box>
      {Conflicts($, { el, sel: { kind: 'version', id: a.id }, detail, mine: name })}
      {ui.pick?.kind === 'version' && ui.pick.id === a.id ? (
        <Box flexDirection="column">
          <Text color="claude" wrap="wrap">
            Press ← here on a main commit to move {name} onto, or on a version to stack it on.
          </Text>
          <Button key="cancel-pick" onPress={() => cancelPick($)}>
            Cancel
          </Button>
        </Box>
      ) : (
        <Box flexDirection="column" gap={1}>
          {open && Action({ el, id: "choose", primary: true, label: WORDS.choose, help: HELP.choose, onPress: () => act($, `Chose ${name}.`, () => postJson($, `/api/attempts/${a.id}/promote`)) })}
          {g.conflicts === 0 && stale && (
            Action({ el, id: "update-parent", label: WORDS.updateFromParent(versionName(stale.attempt)), help: HELP.updateFromParent(versionName(stale.attempt)), onPress: () => preview($, a.id, `a${stale.attempt.id}`, versionName(stale.attempt), { isStack: true, isUpdate: true }) })
          )}
          {g.conflicts === 0 && !g.stacked_on && !onBranch && behind > 0 && movable && (
            Action({ el, id: "update-main", label: WORDS.updateFromMain, help: HELP.updateFromMain(behind), onPress: () => preview($, a.id, graph.trunk[0]?.commit, 'main', { isStack: false, isUpdate: true }) })
          )}
          {g.conflicts === 0 && movable && Action({ el, id: "rebase-onto", label: WORDS.rebaseOnto, help: HELP.rebaseOnto, onPress: () => startPick($, { kind: 'version', id: a.id }) })}
          {movable && g.conflicts === 0 && (
            Action({ el, id: "open-pr", label: g.pr ? 'Update PR' : WORDS.openPr, help: g.pr ? HELP.updatePr : parent ? HELP.openPrStacked(versionName(parent.attempt)) : HELP.openPr, onPress: () => act($, '', async () => { const r = (await postJson($, `/api/attempts/${a.id}/pr`, { draft: false })) as { pr: { number: number; updated: boolean } }; $.ui.toast(`${r.pr.updated ? 'Updated' : 'Opened'} PR #${r.pr.number}.`) })  })
          )}
          {isRunning && Action({ el, id: "stop", label: "Stop", help: "Stops the agent running in this version. Its work so far stays.", onPress: () => act($, `Asked ${name} to stop.`, () => postJson($, `/api/attempts/${a.id}/stop`)) })}
          {open &&
            (ui.discarding ? (
              <Input
                key="discard-reason"
                label={`${WORDS.discard}:`}
                placeholder="Why set it aside?"
                submitLabel="discard"
                autoFocus
                onSubmit={reason =>
                  reason.trim()
                    ? act($, `Set ${name} aside.`, async () => {
                        await update($, uiAtom, u => ({ ...u, discarding: false, sel: null }))
                        await postJson($, `/api/attempts/${a.id}/abandon`, { reason: reason.trim() })
                      })
                    : undefined
                }
              />
            ) : (
              Action({ el, id: "discard", label: WORDS.discard, help: HELP.discard, onPress: () => update($, uiAtom, u => ({ ...u, discarding: true })) })
            ))}
        </Box>
      )}
      {Files({ el, detail, more: link })}
      <Link href={link}>Open in the browser</Link>
    </Box>
  )
}

function BranchDetail($: $, { el, ui, detail, w, name }: { el: El; ui: AgentvcUi; detail: AgentvcDetail; w: WorkspaceInfo; name: string }) {
  const { Box, Text, Button, Link } = el
  const b = w.branches.find(x => x.name === name)!
  const readOnly = w.kind === 'remote'
  const stoppedHere = w.pending?.branch === name
  const lastOk = Math.max(0, ...w.requests.filter(r => r.status === 'applied').map(r => r.created_at))
  const failed = w.requests.find(r => r.status === 'failed' && r.branch === name && r.created_at > lastOk && r.code === 'conflict')
  const link = `${webBase()}/review/b/${enc(w.id)}/${name.split('/').map(enc).join('/')}`
  return (
    <Box flexDirection="column" gap={1}>
      <Box flexDirection="column">
        <Text bold mono>
          {name}
        </Text>
        <Text dimColor size="caption">
          {b.commits} commit{b.commits === 1 ? '' : 's'} on {b.parent}
          {b.current ? ' · checked out' : ''} · {w.name}
        </Text>
      </Box>
      {stoppedHere && (
        <Box flexDirection="column" gap={1}>
          <Text color="error" wrap="wrap">
            {name} clashes with {w.pending!.onto.replace(/^origin\//, '')}.
          </Text>
          {Conflicts($, { el, sel: { kind: 'branch', workspace: w.id, name }, detail, mine: name })}
          <Box flexDirection="row" gap={1}>
            <Button key="ws-continue" variant="primary" onPress={() => finish($, w, false)}>
              Continue
            </Button>
            <Button key="ws-abort" onPress={() => finish($, w, true)}>
              Abort
            </Button>
          </Box>
        </Box>
      )}
      {!stoppedHere && failed && !readOnly && (
        Action({ el, id: "retry", primary: true, label: WORDS.resolve, help: `${failed.op} ${name} clashed with its parent. Runs it again and stops at the clash so you can settle it here.`, onPress: () => stackEdit($, w, failed.op === 'move' && failed.onto ? { op: 'move', branch: name, onto: failed.onto } : { op: 'restack', branch: name }, `${failed.op} ${name} done.`)  })
      )}
      {ui.pick?.kind === 'branch' && ui.pick.name === name ? (
        <Box flexDirection="column">
          <Text color="claude" wrap="wrap">
            Press ← here on a branch, or on main, to move {name} there.
          </Text>
          <Button key="cancel-pick" onPress={() => cancelPick($)}>
            Cancel
          </Button>
        </Box>
      ) : (
        !readOnly &&
        !w.pending && (
          <Box flexDirection="column" gap={1}>
            {b.needs_restack && b.conflicts === 0 && (
              Action({ el, id: "restack", primary: true, label: WORDS.restack, help: HELP.restack, onPress: () => stackEdit($, w, { op: 'restack', branch: name }, `Restacked ${name}.`) })
            )}
            {!b.current && Action({ el, id: "checkout", label: WORDS.checkout, help: HELP.checkout, onPress: () => stackEdit($, w, { op: 'checkout', branch: name }, `Checked out ${name} in ${w.name}.`) })}
            {Action({ el, id: "move-onto", label: WORDS.moveOnto, help: HELP.moveBranch, onPress: () => startPick($, { kind: 'branch', workspace: w.id, name }) })}
          </Box>
        )
      )}
      {detail.commits.length > 0 && (
        <Box flexDirection="column">
          <Text bold dimColor size="caption">
            COMMITS
          </Text>
          {detail.commits.map(c => (
            <Text key={`c-${c.commit}`} wrap="end">
              <Text dimColor mono>
                {shortSha(c.commit)}
              </Text>{' '}
              {c.subject}
            </Text>
          ))}
        </Box>
      )}
      {Files({ el, detail, more: link })}
      <Link href={link}>Open in the browser</Link>
    </Box>
  )
}

function PreviewCard($: $, { el, board, ui }: { el: El; board: AgentvcBoard; ui: AgentvcUi }) {
  const { Box, Text, Button } = el
  const p = ui.preview!
  return (
    <Box flexDirection="column" gap={1} borderStyle="round" paddingX={1}>
      <Text bold wrap="wrap">
        {previewTitle(board, p)}
      </Text>
      {p.error ? (
        <Text color="error" wrap="wrap">
          {p.error}
        </Text>
      ) : (
        <Box flexDirection="column">
          {p.clash.length === 0 ? (
            <Text color="success">Merges cleanly ({p.clean.length} file{p.clean.length === 1 ? '' : 's'}).</Text>
          ) : (
            <Text color="warning" wrap="wrap">
              {p.clash.length} file{p.clash.length === 1 ? '' : 's'} clash; you pick what to keep after.
            </Text>
          )}
          {p.clash.map(f => (
            <Text key={`pc-${f}`} mono color="error">
              ! {f}
            </Text>
          ))}
          {p.clean.slice(0, 8).map(f => (
            <Text key={`pk-${f}`} mono dimColor>
              ✓ {f}
            </Text>
          ))}
        </Box>
      )}
      <Box flexDirection="row" gap={1}>
        {!p.error && (
          <Button key="apply-move" variant="primary" onPress={() => applyPreview($)}>
            {p.isUpdate ? 'Update' : p.isStack ? 'Stack' : 'Rebase'}
          </Button>
        )}
        <Button key="cancel-move" onPress={() => cancelPick($)}>
          Cancel
        </Button>
      </Box>
    </Box>
  )
}

export const register: Register = (on, options) => {
  base = String(options.url ?? base).replace(/\/+$/, '')
  pollMs = Math.max(1, Number(options.pollSeconds ?? 2)) * 1000
  autoOpen = options.autoOpen !== false

  on('session.start', async ($, e, next) => {
    cwd = e.cwd
    await $.command.register({
      name: 'agentvc',
      description: 'Show the agentvc branch tree (main, your stacks, agent versions) in a side pane',
    })
    $.clock.every(pollMs, () => void poll($))
    void poll($)
    return next(e)
  })

  on('command.run', { command: 'agentvc' }, async $ => {
    hasOpened = true
    const opened = await $.ui.open({ id: PANE, title: TITLE, focus: true })
    void poll($)
    return { text: opened.isPlaced ? 'agentvc pane opened.' : 'agentvc pane is waiting for room.' }
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const el = $.ui.resolve(e)
    const { Box, Text, Button, Link } = el
    const board = await read($, boardAtom)
    const ui = await read($, uiAtom)
    const detail = await read($, detailAtom)

    // The minimized form: one glance.
    if (e.props.placement === 'widget') {
      if (!board.isReachable) return <Text dimColor>agentvc: daemon not running</Text>
      const attention =
        (board.graph?.attempts.filter(g => g.conflicts > 0).length ?? 0) +
        board.workspaces.reduce((n, w) => n + (w.pending ? 1 : 0) + w.branches.filter(b => b.needs_restack).length, 0)
      return (
        <Box flexDirection="column">
          <Text bold>agentvc · {board.space}</Text>
          <Text color={board.running.length > 0 ? 'claude' : 'inactive'}>{board.running.length} running</Text>
          <Text color={attention > 0 ? 'warning' : 'inactive'}>{attention} need attention</Text>
        </Box>
      )
    }

    if (!board.isReachable && board.fetchedAt > 0 && !board.graph) {
      return (
        <Box flexDirection="column" gap={1}>
          <Text color="warning">agentvcd is not answering at {base}</Text>
          <Text dimColor>Start it in your repository:</Text>
          <Text mono>agentvcd</Text>
          {board.error && (
            <Text dimColor size="caption" wrap="wrap">
              {board.error}
            </Text>
          )}
          <Button key="retry" onPress={() => poll($)}>
            Retry
          </Button>
        </Box>
      )
    }
    if (board.fetchedAt === 0 || !board.graph) return <Text dimColor>Connecting to agentvcd…</Text>

    const rows = paneRows(board, ui)
    const closed = closedCount(board)
    const sel = ui.sel
    const g = sel?.kind === 'version' ? findVersion(board, sel.id) : undefined
    const br = sel?.kind === 'branch' ? findBranch(board, sel.workspace, sel.name) : undefined
    const anyTarget = rows.some(r => r.isTarget)

    return (
      <Box flexDirection="column" gap={1}>
        <Box flexDirection="row" justifyContent="space-between">
          <Text bold>{board.space}</Text>
          <Text color={board.isReachable ? 'success' : 'warning'} size="caption">
            {board.isReachable ? '● live' : '○ daemon not answering, showing last state'}
          </Text>
        </Box>

        {ui.pick && (
          <Box flexDirection="row" gap={1}>
            <Text color="claude" wrap="wrap">
              {anyTarget
                ? `Moving ${ui.pick.kind === 'version' ? nameOf(board, ui.pick.id) : ui.pick.name}: press ← here.`
                : 'Nothing to move it onto.'}
            </Text>
            <Button key="cancel-pick-top" dimColor onPress={() => cancelPick($)}>
              Cancel
            </Button>
          </Box>
        )}
        {ui.preview && PreviewCard($, { el, board, ui })}

        {Tree($, { el, rows, ui })}
        {board.graph.attempts.length <= 1 && board.workspaces.every(w => w.branches.length === 0) && (
          <Text dimColor wrap="wrap">
            No versions or stacked branches yet. Agents add versions with `agentvc fork`; stack a branch with `agentvc stack create`.
          </Text>
        )}
        {closed > 0 && (
          <Button key="toggle-closed" plain dimColor onPress={() => update($, uiAtom, u => ({ ...u, showClosed: !u.showClosed }))}>
            {ui.showClosed ? `Hide ${closed} not chosen` : `Show ${closed} not chosen`}
          </Button>
        )}

        {g && VersionDetail($, { el, board, ui, detail, g })}
        {br && BranchDetail($, { el, ui, detail, w: br.w, name: br.b.name })}
        {detail.error && sel && (
          <Text color="error" size="caption" wrap="wrap">
            {detail.error}
          </Text>
        )}

        <Box flexDirection="row" gap={1}>
          <Button key="undo" dimColor onPress={() => act($, 'Undone.', () => postJson($, '/api/undo', {}))}>
            {WORDS.undo}
          </Button>
          <Button key="refresh" dimColor onPress={() => poll($)}>
            Refresh
          </Button>
          <Link href={`${webBase()}/review`}>Open review UI</Link>
        </Box>
      </Box>
    )
  })
}
