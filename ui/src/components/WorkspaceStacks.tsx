// A window's stack of branches (each worktree, each editor window has its
// own), drawn inside the one branch tree on the main commit it sits on. Drag
// a branch onto another branch, or onto a main commit, to move it there; the
// window that owns it makes the change, at once when idle or after its agent
// stops.

import { useMemo, useState, type DragEvent, type ReactNode } from 'react'
import { useLocation } from 'react-router'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { WorkspaceInfo, WsBranch, WsOp, WsRequest } from '../api/types'
import { branchLink, selectionFromPath, useGo } from '../core/links'
import { canMoveBranch } from '../core/graph'
import { prWords, shortSha } from '../core/words'
import { timeAgo } from '../lib/format'
import { BRANCH_MIME, useStackEdit } from './stackEdit'
import { useToast } from './Toaster'

export function WindowStack({
  w,
  mine,
  showHead,
  under,
}: {
  w: WorkspaceInfo
  mine: boolean
  /** Name the window: there is more than one. */
  showHead: boolean
  /** Rows drawn under a branch: the versions based on its tip. */
  under: (b: WsBranch) => ReactNode
}) {
  const go = useGo()
  const toast = useToast()
  const { refresh } = useLive()
  const { run, busy } = useStackEdit()
  const [drag, setDrag] = useState<string | null>(null)
  const [over, setOver] = useState<string | null>(null)
  const readOnly = w.kind === 'remote'
  const byName = useMemo(() => new Map(w.branches.map((b) => [b.name, b])), [w])
  const queued = w.requests.filter((r) => r.status === 'queued')
  // Failures since the last edit that worked; older ones are history.
  const lastOk = Math.max(0, ...w.requests.filter((r) => r.status === 'applied').map((r) => r.created_at))
  const failed = w.requests.filter((r) => r.status === 'failed' && r.created_at > lastOk).slice(0, 1)
  const sel = selectionFromPath(useLocation().pathname)
  const selected = (name: string) => sel?.kind === 'branch' && sel.workspace === w.id && sel.name === name

  if (w.branches.length === 0 && !w.pending && queued.length === 0) return null

  /** Run a failed edit again, stopping at the clash so it can be resolved here. */
  const retry = (r: WsRequest) => {
    const op: WsOp | null =
      r.op === 'move' && r.branch && r.onto
        ? { op: 'move', branch: r.branch, onto: r.onto }
        : r.op === 'restack'
          ? { op: 'restack', branch: r.branch }
          : r.op === 'delete' && r.branch
            ? { op: 'delete', branch: r.branch }
            : null
    return op ? run(w, op, `${r.op} ${r.branch ?? ''} done.`, true) : Promise.resolve()
  }

  const dropProps = (onto: string) => ({
    onDragOver: (e: DragEvent) => {
      if (!drag || !canMoveBranch(w, drag, onto)) return
      e.preventDefault()
      e.stopPropagation()
      e.dataTransfer.dropEffect = 'move'
      setOver(onto)
    },
    onDragLeave: () => setOver((o) => (o === onto ? null : o)),
    onDrop: (e: DragEvent) => {
      e.preventDefault()
      e.stopPropagation()
      const branch = drag
      setDrag(null)
      setOver(null)
      if (!branch || !canMoveBranch(w, branch, onto)) return
      void run(w, { op: 'move', branch, onto }, `Moved ${branch} onto ${onto}${byName.get(branch)?.children.length ? ', with the branches above it' : ''}.`, true)
    },
  })

  return (
    <div className={`wst ${busy ? 'busy' : ''}`} role="group" aria-label={`${w.name} stack`}>
      {showHead && (
        <div className="wst-head" title={`${w.path}${w.machine ? ` · ${w.machine}` : ''}`}>
          <span className="bname">
            {w.name}
            {mine ? <span className="muted"> · this window</span> : null}
          </span>
          {w.state === 'agent_running' && <span className="bbadge bbadge-run">agent working</span>}
          {readOnly && <span className="bbadge">read-only · {w.machine}</span>}
          {w.last_seen ? <span className="bsub">{timeAgo(w.last_seen)}</span> : null}
        </div>
      )}
      {!showHead && w.state === 'agent_running' && (
        <div className="wst-head">
          <span className="bbadge bbadge-run">agent working</span>
        </div>
      )}
      {w.pending && (
        <div className="wst-req wst-req-bad">
          <span className="bbadge bbadge-conf">stopped</span>
          <span className="wst-req-text">{w.pending.branch} clashes with its parent.</span>
          <button type="button" className="linkish" onClick={() => go(branchLink(w.id, w.pending!.branch))}>
            Resolve
          </button>
        </div>
      )}
      {w.branches.map((b) => (
        <div key={b.name}>
          <BranchRow
            b={b}
            movable={!readOnly}
            stopped={!!w.pending}
            dragging={drag === b.name}
            over={over === b.name}
            dropProps={dropProps(b.name)}
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
            selected={selected(b.name)}
            onOpen={() => go(branchLink(w.id, b.name))}
            onCheckout={() => !b.current && void run(w, { op: 'checkout', branch: b.name }, `Checked out ${b.name} in ${w.name}.`)}
            onRestack={() => void run(w, { op: 'restack', branch: b.name }, `Restacked ${b.name}.`, true)}
          />
          {under(b)}
        </div>
      ))}
      {queued.map((r) => (
        <div key={r.id} className="wst-req">
          <span className="bbadge bbadge-run">queued</span>
          <span className="wst-req-text">
            {r.op} {r.branch ?? ''}
            {r.onto ? ` onto ${r.onto}` : ''}
          </span>
          <button type="button" className="linkish" onClick={() => void api.cancelRequest(r.id).then(refresh, (e: unknown) => toast.error(e))}>
            Cancel
          </button>
        </div>
      ))}
      {!w.pending &&
        failed.map((r) => (
          <div key={r.id} className="wst-req wst-req-bad" title={r.reason ?? r.code}>
            <span className="bbadge bbadge-conf">failed</span>
            <span className="wst-req-text">
              {r.op} {r.branch ?? ''}: {r.code === 'conflict' ? 'clashes with its parent' : (r.reason ?? r.code)}
            </span>
            {r.code === 'conflict' && !readOnly && (
              <button type="button" className="linkish" onClick={() => void retry(r)}>
                Resolve
              </button>
            )}
          </div>
        ))}
    </div>
  )
}

function BranchRow({
  b,
  movable,
  stopped,
  dragging,
  over,
  dropProps,
  onDragStart,
  onDragEnd,
  selected,
  onOpen,
  onCheckout,
  onRestack,
}: {
  b: WsBranch
  movable: boolean
  /** A restack in this window stopped on a clash: nothing else runs until it is resolved. */
  stopped: boolean
  dragging: boolean
  over: boolean
  dropProps: Record<string, unknown>
  onDragStart: (e: DragEvent) => void
  onDragEnd: () => void
  selected: boolean
  onOpen: () => void
  onCheckout: () => void
  onRestack: () => void
}) {
  const dot = b.conflicts > 0 ? 'conf' : b.status === 'running' ? 'run' : b.pr?.state === 'merged' ? 'win' : b.needs_restack ? 'mute' : 'ok'
  return (
    <div
      role="treeitem"
      aria-selected={selected}
      className={`brow brow-ver wst-branch ${b.current ? 'cur' : ''} ${selected ? 'sel' : ''} ${dragging ? 'dragging' : ''} ${over ? 'drop' : ''}`}
      style={{ paddingLeft: `${24 + Math.max(0, b.depth) * 14}px` }}
      draggable={movable}
      onDragStart={onDragStart}
      onDragEnd={onDragEnd}
      onClick={onOpen}
      onDoubleClick={onCheckout}
      {...dropProps}
      title={`${b.name}${b.tip ? ` · ${shortSha(b.tip)}` : ''} · ${b.commits} commit${b.commits === 1 ? '' : 's'} on ${b.parent}\nClick to open · double-click to check out · drag onto a branch or a main commit to move`}
    >
      <span className="wst-elbow" aria-hidden>
        └
      </span>
      <span className={`bdot bdot-${dot}`} aria-hidden />
      <span className="blabel">
        <span className="bname mono">{b.name}</span>
        {b.current && <span className="bbadge">checked out</span>}
        {b.conflicts > 0 ? (
          <span className="bbadge bbadge-conf">
            {b.conflicts} conflict{b.conflicts === 1 ? '' : 's'}
          </span>
        ) : b.needs_restack ? (
          <span className="bbadge bbadge-behind">needs restack</span>
        ) : b.behind > 0 && b.parent_kind === 'trunk' ? (
          <span className="bbadge bbadge-behind">↓{b.behind}</span>
        ) : null}
        {b.pr && <PrBadge pr={b.pr} />}
        {b.parent_kind === 'other_window' && <span className="bbadge">on {b.parent_workspace ?? 'another window'}</span>}
      </span>
      {movable && b.needs_restack && b.conflicts === 0 && !stopped && (
        <span className="bhover" onClick={(e) => e.stopPropagation()}>
          <button type="button" className="bicon" title="Restack onto its parent" aria-label="Restack" onClick={onRestack}>
            ↻
          </button>
        </span>
      )}
    </div>
  )
}

/** `#12` with what the PR needs, e.g. `#12 · checks fail`. */
function PrBadge({ pr }: { pr: NonNullable<WsBranch['pr']> }) {
  const w = prWords(pr.state, pr.status)
  return (
    <span className={`bbadge bbadge-pr${w ? ` bbadge-pr-${w.tone}` : ''}`} title={w ? `PR #${pr.number}: ${w.label}` : `PR #${pr.number}`}>
      #{pr.number}
      {w ? ` · ${w.short}` : ''}
    </span>
  )
}
