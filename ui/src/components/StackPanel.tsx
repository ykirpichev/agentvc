// The sidebar: one window's stack, Graphite style (newest branch on top, main
// at the bottom), with Sync and Undo; the other windows fold into one line
// each, and agent work into a short list below. Click a branch to open it,
// double-click to check it out, drag it onto another branch or main to move it.

import { useMemo, useState, type DragEvent } from 'react'
import { useLocation } from 'react-router'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { TaskSummary, WorkspaceInfo, WsBranch } from '../api/types'
import { useResource } from '../api/useResource'
import { thisWorkspace } from '../core/embed'
import { canMoveBranch } from '../core/graph'
import { branchLink, selectionFromPath, taskLink, useGo } from '../core/links'
import { mainAhead, needsSync, stackRows, trunkName, windowState } from '../core/stack'
import { prWords, shortSha } from '../core/words'
import { plural } from '../lib/format'
import { BRANCH_MIME, useStackEdit } from './stackEdit'
import { useToast } from './Toaster'
import './stack.css'

const FOLD_KEY = 'agentvc.folded'

function savedFolds(): Set<string> {
  try {
    return new Set(JSON.parse(sessionStorage.getItem(FOLD_KEY) ?? '[]') as string[])
  } catch {
    return new Set()
  }
}

/** A branch whose pull request merged: Sync drops it, so the stack leaves it out. */
const merged = (b: WsBranch) => b.pr?.state === 'merged'

export function StackPanel({ tasks }: { tasks: TaskSummary[] | undefined }) {
  const ws = useResource('workspaces', api.workspaces)
  const [folded, setFolded] = useState<Set<string>>(savedFolds)
  const sel = selectionFromPath(useLocation().pathname)
  const toggle = (id: string) =>
    setFolded((f) => {
      const n = new Set(f)
      if (n.has(id)) n.delete(id)
      else n.add(id)
      try {
        sessionStorage.setItem(FOLD_KEY, JSON.stringify([...n]))
      } catch {
        // The choice just doesn't survive a reload.
      }
      return n
    })

  const list = ws.data && !ws.error ? ws.data.workspaces : []
  const here = ws.data && !ws.error ? thisWorkspace(ws.data) : undefined
  // This editor window first, then windows with a stack, then the empty ones.
  const live = (w: WorkspaceInfo) => w.branches.some((b) => !merged(b)) || w.pending !== null || w.state === 'agent_running'
  const ordered = [...list].sort((a, b) => Number(b === here) - Number(a === here) || Number(live(b)) - Number(live(a)))
  // Tasks whose tries belong to a branch show on that branch; the rest are listed below.
  const onBranch = new Set(list.flatMap((w) => w.branches.map((b) => b.task).filter((t): t is number => t !== null)))
  const tries = new Map((tasks ?? []).map((t) => [t.id, t.attempts.total - t.attempts.abandoned]))
  const work = (tasks ?? []).filter((t) => !onBranch.has(t.id) && t.attempts.total - t.attempts.abandoned > 0 && t.attempts.promoted === 0)

  if (ws.error && !work.length) return <div className="sp sp-empty muted">No workspaces here: {ws.error.message}</div>
  if (!ws.data && !ws.error) return <div className="sp sp-empty muted">Loading…</div>

  return (
    <div className="sp">
      <h3 className="sp-h sp-h-top">
        Workspaces <span className="sp-count">{list.length}</span>
      </h3>
      {ordered.length === 0 && (
        <p className="sp-empty muted">
          No workspaces yet. Each git worktree (each editor window) is a workspace with its own stack: run <code>agentvc stack create</code> in one.
        </p>
      )}
      {ordered.map((w) => (
        <WindowStack
          key={w.id}
          w={w}
          mine={w === here}
          tries={tries}
          open={!folded.has(w.id) || (sel?.kind === 'branch' && sel.workspace === w.id)}
          onToggle={() => toggle(w.id)}
        />
      ))}

      {work.length > 0 && (
        <section className="sp-sec" aria-label="Agent work">
          <h3 className="sp-h">Agent work</h3>
          {work.map((t) => (
            <AgentLine key={t.id} t={t} />
          ))}
        </section>
      )}
    </div>
  )
}

function AgentLine({ t }: { t: TaskSummary }) {
  const go = useGo()
  const sel = selectionFromPath(useLocation().pathname)
  const open = t.attempts.total - t.attempts.abandoned
  return (
    <button type="button" className={`sp-line ${sel?.kind === 'task' && sel.id === t.id ? 'sel' : ''}`} onClick={() => go(taskLink(t.id))}>
      <span className="sp-line-name">{t.goal}</span>
      {t.attempts.exploring > 0 ? (
        <span className="sp-chip sp-chip-run">working</span>
      ) : (
        <span className="sp-chip sp-chip-mute">{`${open} ${open === 1 ? 'try' : 'tries'}`}</span>
      )}
    </button>
  )
}

function WindowStack({
  w,
  mine,
  tries,
  open,
  onToggle,
}: {
  w: WorkspaceInfo
  mine: boolean
  /** Open tries per task, for the "N tries" chip on a branch made from one. */
  tries: Map<number, number>
  open: boolean
  onToggle: () => void
}) {
  const go = useGo()
  const toast = useToast()
  const { refresh } = useLive()
  const { run, busy, submit } = useStackEdit()
  const [drag, setDrag] = useState<string | null>(null)
  const [over, setOver] = useState<string | null>(null)
  const sel = selectionFromPath(useLocation().pathname)
  const live = useMemo(() => w.branches.filter((b) => !merged(b)), [w])
  const gone = w.branches.length - live.length
  const rows = useMemo(() => stackRows({ branches: live }), [live])
  const readOnly = w.kind === 'remote'
  const stopped = w.pending !== null || w.branches.some((b) => b.conflicts > 0)
  const ahead = mainAhead(w)
  const sync = needsSync(w)
  const st = windowState(w)
  // An empty workspace is one line: nothing to show or do until it has a branch.
  const body = open && (rows.length > 0 || stopped || gone > 0)
  const trunk = trunkName(w)
  const queued = w.requests.filter((r) => r.status === 'queued')
  const lastOk = Math.max(0, ...w.requests.filter((r) => r.status === 'applied').map((r) => r.created_at))
  const failed = w.requests.filter((r) => r.status === 'failed' && r.op !== 'submit' && r.created_at > lastOk).slice(0, 1)
  const stoppedAt = w.pending?.branch ?? w.branches.find((b) => b.conflicts > 0)?.name

  const doSync = () => void run(w, { op: 'sync' }, `Synced ${w.name} with ${trunk}.`, true)
  const restackFirst = w.branches.some((b) => b.needs_restack)
  const doSubmit = async () => {
    const r = await submit(w)
    if (r) toast.ok(r, { undo: false })
  }
  const doUndo = () => void run(w, { op: 'undo' }, 'Undone.')

  const dropOn = (onto: string) => ({
    onDragOver: (e: DragEvent) => {
      if (!drag || readOnly || !canMoveBranch(w, drag, onto)) return
      e.preventDefault()
      e.dataTransfer.dropEffect = 'move'
      setOver(onto)
    },
    onDragLeave: () => setOver((o) => (o === onto ? null : o)),
    onDrop: (e: DragEvent) => {
      e.preventDefault()
      const branch = drag
      setDrag(null)
      setOver(null)
      if (!branch || !canMoveBranch(w, branch, onto)) return
      const name = onto === w.trunk ? trunk : onto
      void run(w, { op: 'move', branch, onto }, `Moved ${branch} onto ${name}.`, true)
    },
  })

  return (
    <section className={`sp-win ${busy ? 'busy' : ''}`} aria-label={`${w.name} stack`}>
      <header className="sp-head">
        <button type="button" className="sp-ws" onClick={onToggle} aria-expanded={open} title={w.path}>
          <span className={`sp-caret ${open ? 'open' : ''}`} aria-hidden>
            ▸
          </span>
          <span className="sp-ws-name">{w.name}</span>
          {mine && <span className="sp-here">this window</span>}
          {w.kind === 'remote' && <span className="sp-here">{w.machine}</span>}
          <span className="sp-ws-meta">
            {rows.length ? `${rows.length} branch${rows.length === 1 ? '' : 'es'} on ${trunk}` : `no unmerged branches · on ${trunk}`}
            {sync && !stoppedAt && rows.length > 0 && (
              <span className="sp-warn">{ahead > 0 ? ` · ${trunk} +${ahead}, sync` : ' · sync to rebase'}</span>
            )}
          </span>
        </button>
        {!open && st && <span className={`sp-chip sp-chip-${st.tone}`}>{st.label}</span>}
        {body && !readOnly && (
          <span className="sp-tools">
            <button type="button" className="sp-btn sp-btn-ghost" disabled={busy || stopped} onClick={doUndo} title="Undo the last stack change in this workspace" aria-label={`Undo in ${w.name}`}>
              ↶
            </button>
            <button type="button" className={`sp-btn ${sync && !stopped ? 'sp-btn-pri' : ''}`} disabled={busy || stopped} onClick={doSync} title={`Fetch ${trunk}, drop merged branches, rebase this stack`}>
              Sync
            </button>
          </span>
        )}
      </header>
      {body && (
        <>

      {stoppedAt ? (
        <div className="sp-banner sp-banner-bad">
          <span>
            Stopped at <b>{stoppedAt}</b>: a file changed on both sides
          </span>
          <button type="button" className="sp-btn" onClick={() => go(branchLink(w.id, stoppedAt))}>
            Resolve
          </button>
        </div>
      ) : null}
      {w.state === 'agent_running' && <div className="sp-note">An agent is working here; changes wait until it stops.</div>}

      <div className="sp-rows" role="tree" aria-label={`${w.name} branches`}>

        {rows.map(({ branch: b, lane }) => (
          <BranchRow
            key={b.name}
            b={b}
            lane={lane}
            tries={b.task !== null ? (tries.get(b.task) ?? 0) : 0}
            onTries={() => b.task !== null && go(taskLink(b.task))}
            movable={!readOnly && !stopped}
            selected={sel?.kind === 'branch' && sel.workspace === w.id && sel.name === b.name}
            dragging={drag === b.name}
            over={over === b.name}
            drop={dropOn(b.name)}
            onDragStart={(e) => {
              e.dataTransfer.effectAllowed = 'move'
              e.dataTransfer.setData('text/plain', b.name)
              e.dataTransfer.setData(BRANCH_MIME, JSON.stringify({ workspace: w.id, branch: b.name }))
              setDrag(b.name)
            }}
            onDragEnd={() => {
              setDrag(null)
              setOver(null)
            }}
            onOpen={() => go(branchLink(w.id, b.name))}
            onCheckout={() => !b.current && !readOnly && void run(w, { op: 'checkout', branch: b.name }, `Checked out ${b.name}.`)}
          />
        ))}
        {(rows.length > 0 || w.base) && (
          <div className={`sp-row sp-trunk ${over === w.trunk ? 'drop' : ''}`} {...dropOn(w.trunk)} title={w.base ? `${w.base_ref} · ${shortSha(w.base)}` : w.base_ref}>
            <span className="sp-gutter" aria-hidden>
              <i className="sp-dot sp-dot-trunk" />
            </span>
            <span className="sp-name">{trunk}</span>
          </div>
        )}
      </div>

      {gone > 0 && <div className="sp-note">{`${gone} merged branch${gone === 1 ? '' : 'es'} hidden. Sync drops ${gone === 1 ? 'it' : 'them'}.`}</div>}
      {!readOnly && rows.length > 0 && (
        <div className="sp-foot">
          <button
            type="button"
            className="sp-btn sp-btn-pri"
            disabled={busy || stopped || restackFirst}
            onClick={() => void doSubmit()}
            title={restackFirst ? 'Sync first: some branches are not on their parent yet' : 'Push every branch and open or update one pull request per branch'}
          >
            Submit stack
          </button>
        </div>
      )}

      {queued.map((r) => (
        <div key={r.id} className="sp-note">
          Waiting for the agent: {r.op} {r.branch ?? ''}
          {r.onto ? ` onto ${r.onto}` : ''}.{' '}
          <button type="button" className="linkish" onClick={() => void api.cancelRequest(r.id).then(refresh, (e: unknown) => toast.error(e))}>
            Cancel
          </button>
        </div>
      ))}
      {!stoppedAt &&
        failed.map((r) => (
          <div key={r.id} className="sp-note sp-note-bad" title={r.reason ?? r.code}>
            {r.op} {r.branch ?? ''} failed: {r.code === 'conflict' ? 'a file changed on both sides' : (r.reason ?? r.code)}
          </div>
        ))}
        </>
      )}
    </section>
  )
}

function BranchRow({
  b,
  lane,
  tries,
  onTries,
  movable,
  selected,
  dragging,
  over,
  drop,
  onDragStart,
  onDragEnd,
  onOpen,
  onCheckout,
}: {
  b: WsBranch
  lane: number
  /** Agent tries for the task this branch was made from; a chip when there are several. */
  tries: number
  onTries: () => void
  movable: boolean
  selected: boolean
  dragging: boolean
  over: boolean
  drop: Record<string, unknown>
  onDragStart: (e: DragEvent) => void
  onDragEnd: () => void
  onOpen: () => void
  onCheckout: () => void
}) {
  const pr = b.pr ? prWords(b.pr.state, b.pr.status) : null
  return (
    <div
      role="treeitem"
      aria-selected={selected}
      className={`sp-row ${b.current ? 'cur' : ''} ${selected ? 'sel' : ''} ${dragging ? 'dragging' : ''} ${over ? 'drop' : ''}`}
      style={{ paddingLeft: `${8 + lane * 16}px` }}
      draggable={movable}
      onDragStart={onDragStart}
      onDragEnd={onDragEnd}
      onClick={onOpen}
      onDoubleClick={onCheckout}
      {...drop}
      title={`${b.name} · ${plural(b.commits, 'commit')} on ${b.parent}\nClick to open · double-click to check out · drag onto a branch or main to move`}
    >
      <span className="sp-gutter" aria-hidden>
        <i className={`sp-dot ${b.current ? 'sp-dot-cur' : ''} ${b.conflicts > 0 ? 'sp-dot-bad' : ''}`} />
      </span>
      <span className="sp-name">
        {b.name}
        {b.current && <span className="sp-sub">you're here</span>}
      </span>
      <span className="sp-chips">
        {b.status === 'running' && <span className="sp-chip sp-chip-run">agent</span>}
        {tries > 1 && (
          <button
            type="button"
            className="sp-chip sp-chip-pr sp-chip-btn"
            title="Compare the agent tries for this branch"
            onClick={(e) => {
              e.stopPropagation()
              onTries()
            }}
          >
            {tries} tries
          </button>
        )}
        {b.conflicts > 0 && <span className="sp-chip sp-chip-bad">clash</span>}
        {b.parent_kind === 'other_window' && <span className="sp-chip sp-chip-mute">on {b.parent_workspace ?? 'another window'}</span>}
        {b.pr && (
          <span className={`sp-chip sp-chip-${pr?.tone === 'ok' || pr?.tone === 'win' ? 'ok' : pr?.tone === 'bad' ? 'bad' : 'pr'}`} title={`PR #${b.pr.number}${pr ? `: ${pr.label}` : ''}`}>
            #{b.pr.number}
            {pr ? ` ${pr.short}` : ''}
          </span>
        )}
      </span>
    </div>
  )
}
