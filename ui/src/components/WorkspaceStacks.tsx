// Every workspace (each worktree, each editor window) with its stack of
// branches. "This" shows the window you are in; "All" shows every one. Drag a
// branch onto another branch or onto main to move it there; the workspace
// that owns it makes the change, at once when idle or after its agent stops.

import { useMemo, useState, type DragEvent } from 'react'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { WorkspaceInfo, WsBranch, WsOp } from '../api/types'
import { useResource } from '../api/useResource'
import { thisWorkspace } from '../core/embed'
import { useGo, versionLink } from '../core/links'
import { shortSha } from '../core/words'
import { timeAgo } from '../lib/format'
import { useToast } from './Toaster'

type Scope = 'this' | 'all'
const SCOPE_KEY = 'agentvc.wsScope'

function readScope(): Scope {
  try {
    return localStorage.getItem(SCOPE_KEY) === 'all' ? 'all' : 'this'
  } catch {
    return 'this'
  }
}

export function WorkspaceStacks() {
  const data = useResource('workspaces', api.workspaces)
  const [scope, setScope] = useState<Scope>(readScope)
  const list = data.data
  const mine = list ? thisWorkspace(list) : undefined
  const shown = !list ? [] : scope === 'this' && mine ? [mine] : list.workspaces
  const others = list ? list.workspaces.length - 1 : 0

  const pick = (s: Scope) => {
    setScope(s)
    try {
      localStorage.setItem(SCOPE_KEY, s)
    } catch {
      // Not remembered; fine.
    }
  }

  if (data.error) {
    // Older daemons and spaces outside git have no workspaces; stay quiet.
    return null
  }
  if (!list || list.workspaces.length === 0) return null

  return (
    <section className="wsx" aria-label="Workspaces">
      <div className="bgraph-head">
        <span>Stacks</span>
        <span className="seg" role="tablist" aria-label="Which workspaces">
          <button type="button" role="tab" aria-selected={scope === 'this'} className={scope === 'this' ? 'on' : ''} onClick={() => pick('this')} disabled={!mine}>
            This
          </button>
          <button type="button" role="tab" aria-selected={scope === 'all'} className={scope === 'all' ? 'on' : ''} onClick={() => pick('all')}>
            All{others > 0 ? ` ${list.workspaces.length}` : ''}
          </button>
        </span>
      </div>
      {shown.map((w) => (
        <WorkspaceTree key={w.id} w={w} mine={w.id === mine?.id} showHead={scope === 'all' || !mine} />
      ))}
    </section>
  )
}

function WorkspaceTree({ w, mine, showHead }: { w: WorkspaceInfo; mine: boolean; showHead: boolean }) {
  const toast = useToast()
  const go = useGo()
  const { refresh } = useLive()
  const [drag, setDrag] = useState<string | null>(null)
  const [over, setOver] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const readOnly = w.kind === 'remote'
  const byName = useMemo(() => new Map(w.branches.map((b) => [b.name, b])), [w])
  const queued = w.requests.filter((r) => r.status === 'queued')
  const failed = w.requests.filter((r) => r.status === 'failed').slice(0, 2)
  const trunkName = w.trunk.replace(/^origin\//, '')

  /** Branches below `name` (its upstack): it cannot move onto those. */
  const upstack = (name: string): Set<string> => {
    const out = new Set<string>([name])
    const walk = (n: string) => byName.get(n)?.children.forEach((c) => (out.has(c) ? undefined : (out.add(c), walk(c))))
    walk(name)
    return out
  }
  const canDrop = (branch: string | null, onto: string) => {
    if (!branch || readOnly) return false
    const b = byName.get(branch)
    if (!b || b.parent === onto || (onto === w.trunk && b.parent_kind === 'trunk')) return false
    return !upstack(branch).has(onto)
  }

  const run = async (op: WsOp, done: string) => {
    setBusy(true)
    try {
      const r = await api.stackEdit(w.id, op)
      if (r.outcome === 'queued') toast.ok(`Queued for ${w.name}: ${r.reason ?? 'an agent is working there'}. It applies when the agent stops.`, { undo: false })
      else toast.ok(done, { undo: false })
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }

  const drop = (onto: string) => {
    const branch = drag
    setDrag(null)
    setOver(null)
    if (!branch || !canDrop(branch, onto)) return
    const label = onto === w.trunk ? trunkName : onto
    void run({ op: 'move', branch, onto }, `Moved ${branch} onto ${label}${byName.get(branch)?.children.length ? ', with the branches above it' : ''}.`)
  }

  const dragProps = (onto: string) => ({
    onDragOver: (e: DragEvent) => {
      if (!canDrop(drag, onto)) return
      e.preventDefault()
      e.dataTransfer.dropEffect = 'move'
      setOver(onto)
    },
    onDragLeave: () => setOver((o) => (o === onto ? null : o)),
    onDrop: (e: DragEvent) => {
      e.preventDefault()
      drop(onto)
    },
  })

  return (
    <div className={`wst ${busy ? 'busy' : ''}`}>
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
      <div className={`brow brow-trunk wst-trunk ${over === w.trunk ? 'drop' : ''}`} {...dragProps(w.trunk)} title={w.base ?? w.trunk}>
        <span className="gcell g-trunkdot first last" aria-hidden>
          <i />
        </span>
        <span className="blabel">
          <span className="bname mono">{trunkName}</span>
          {w.base && <span className="bsub">{shortSha(w.base)}</span>}
        </span>
      </div>
      {w.branches.length === 0 && <p className="wst-empty muted">No stacked branches.</p>}
      {w.branches.map((b) => (
        <BranchRow
          key={b.name}
          b={b}
          movable={!readOnly}
          dragging={drag === b.name}
          over={over === b.name}
          dropProps={dragProps(b.name)}
          onDragStart={() => setDrag(b.name)}
          onDragEnd={() => {
            setDrag(null)
            setOver(null)
          }}
          onOpen={() => b.attempt && go(versionLink(b.attempt))}
          onRestack={() => void run({ op: 'restack', branch: b.name }, `Restacked ${b.name}.`)}
        />
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
      {failed.map((r) => (
        <div key={r.id} className="wst-req wst-req-bad" title={r.code}>
          <span className="bbadge bbadge-conf">failed</span>
          <span className="wst-req-text">
            {r.op} {r.branch ?? ''}: {r.reason ?? r.code}
          </span>
        </div>
      ))}
    </div>
  )
}

function BranchRow({
  b,
  movable,
  dragging,
  over,
  dropProps,
  onDragStart,
  onDragEnd,
  onOpen,
  onRestack,
}: {
  b: WsBranch
  movable: boolean
  dragging: boolean
  over: boolean
  dropProps: Record<string, unknown>
  onDragStart: () => void
  onDragEnd: () => void
  onOpen: () => void
  onRestack: () => void
}) {
  const dot = b.conflicts > 0 ? 'conf' : b.status === 'running' ? 'run' : b.pr?.state === 'merged' ? 'win' : b.needs_restack ? 'mute' : 'ok'
  return (
    <div
      role="treeitem"
      aria-selected={b.current}
      className={`brow brow-ver wst-branch ${b.current ? 'cur' : ''} ${dragging ? 'dragging' : ''} ${over ? 'drop' : ''}`}
      style={{ paddingLeft: `${10 + Math.max(0, b.depth) * 14}px` }}
      draggable={movable}
      onDragStart={(e) => {
        e.dataTransfer.effectAllowed = 'move'
        e.dataTransfer.setData('text/plain', b.name)
        onDragStart()
      }}
      onDragEnd={onDragEnd}
      onClick={onOpen}
      {...dropProps}
      title={`${b.name}${b.tip ? ` · ${shortSha(b.tip)}` : ''} · ${b.commits} commit${b.commits === 1 ? '' : 's'} on ${b.parent}`}
    >
      <span className="wst-elbow" aria-hidden>
        └
      </span>
      <span className={`bdot bdot-${dot}`} aria-hidden />
      <span className="blabel">
        <span className="bname mono">
          {b.name}
        </span>
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
        {b.pr && <span className="bbadge bbadge-pr">#{b.pr.number}</span>}
        {b.parent_kind === 'other_window' && <span className="bbadge">on {b.parent_workspace ?? 'another window'}</span>}
        {b.attempt ? <span className="bbadge">a{b.attempt}</span> : null}
      </span>
      {movable && b.needs_restack && (
        <span className="bhover" onClick={(e) => e.stopPropagation()}>
          <button type="button" className="bicon" title="Restack onto its parent" aria-label="Restack" onClick={onRestack}>
            ↻
          </button>
        </span>
      )}
    </div>
  )
}
