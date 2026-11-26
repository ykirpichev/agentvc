// The branch graph: main's commits down the left, each task's versions off
// the commit they are based on. Drag a version onto a main commit to rebase
// it there; the detail pane then previews the move before anything changes.

import { useMemo, useState, type DragEvent } from 'react'
import { useLocation, useNavigate } from 'react-router'
import { commitLink, selectionFromPath, taskLink, usePick, versionLink } from '../core/links'
import type { Graph, GraphAttempt, TaskSummary } from '../api/types'
import { canMoveOnto, layout, type Cell, type GraphRow } from '../core/graph'
import { shortSha, versionName } from '../core/words'
import { timeAgo } from '../lib/format'

function Gutter({ cells }: { cells: Cell[] }) {
  return (
    <>
      {cells.map((c, i) => (
        <span key={i} className={`gcell g-${c}`} aria-hidden />
      ))}
    </>
  )
}

export function BranchGraph({ graph, tasks }: { graph: Graph; tasks: TaskSummary[] }) {
  const navigate = useNavigate()
  const loc = useLocation()
  const sel = selectionFromPath(loc.pathname)
  const pick = usePick()
  const [showClosed, setShowClosed] = useState(false)
  const [drag, setDrag] = useState<number | null>(null)
  const [over, setOver] = useState<string | null>(null)
  const rows = useMemo(() => layout(graph, tasks, { showClosed }), [graph, tasks, showClosed])
  const byId = useMemo(() => new Map(graph.attempts.map((g) => [g.attempt.id, g])), [graph])
  const taskName = useMemo(() => new Map(tasks.map((t) => [t.id, t.goal])), [tasks])
  const closed = graph.attempts.filter((g) => g.attempt.status === 'abandoned').length
  const moving = drag ?? pick
  const movingG = moving === null ? undefined : byId.get(moving)

  const moveTo = (id: number, commit: string) => {
    setDrag(null)
    setOver(null)
    if (canMoveOnto(byId.get(id), commit)) navigate(versionLink(id, commit))
  }

  const onDragOver = (e: DragEvent, commit: string) => {
    if (drag === null || !canMoveOnto(byId.get(drag), commit)) return
    e.preventDefault()
    e.dataTransfer.dropEffect = 'move'
    setOver(commit)
  }

  const isSel = (r: GraphRow) =>
    (r.kind === 'version' && sel?.kind === 'version' && sel.id === r.id) ||
    (r.kind === 'task' && sel?.kind === 'task' && sel.id === r.task) ||
    (r.kind === 'trunk' && sel?.kind === 'commit' && r.commit.startsWith(sel.commit))

  return (
    <div className="bgraph">
      <div className="bgraph-head">
        <span>Branches</span>
        {closed > 0 && (
          <button type="button" className="linkish" onClick={() => setShowClosed((s) => !s)}>
            {showClosed ? 'Hide' : 'Show'} {closed} not chosen
          </button>
        )}
      </div>
      <div className="bgraph-rows" role="tree" aria-label="Branches">
        {rows.map((r) => {
          if (r.kind === 'trunk') {
            const c = graph.trunk.find((t) => t.commit === r.commit)!
            const target = movingG && canMoveOnto(movingG, r.commit)
            return (
              <div
                key={r.key}
                role="treeitem"
                aria-selected={isSel(r)}
                className={`brow brow-trunk ${isSel(r) ? 'sel' : ''} ${over === r.commit ? 'drop' : ''} ${pick !== null && target ? 'target' : ''}`}
                onClick={() => (pick !== null && target ? moveTo(pick, r.commit) : navigate(commitLink(r.commit)))}
                onDragOver={(e) => onDragOver(e, r.commit)}
                onDragLeave={() => setOver((o) => (o === r.commit ? null : o))}
                onDrop={(e) => {
                  e.preventDefault()
                  if (drag !== null) moveTo(drag, r.commit)
                }}
                title={`${c.commit}\n${c.summary}`}
              >
                <span className={`gcell g-trunkdot ${r.first ? 'first' : ''} ${r.last ? 'last' : ''}`} aria-hidden>
                  <i />
                </span>
                <span className="blabel">
                  <span className="bname mono">{c.summary || shortSha(c.commit)}</span>
                  <span className="bsub">
                    {r.first && c.on_upstream ? `${graph.upstream} · ` : ''}
                    {c.on_upstream ? '' : 'not on main · '}
                    {shortSha(c.commit)} · {timeAgo(c.time)}
                  </span>
                </span>
              </div>
            )
          }
          if (r.kind === 'task') {
            const name = r.task === null ? 'No task' : (taskName.get(r.task) ?? `Task ${r.task}`)
            return (
              <div
                key={r.key}
                role="treeitem"
                aria-selected={isSel(r)}
                className={`brow brow-task ${isSel(r) ? 'sel' : ''}`}
                onClick={() => r.task !== null && navigate(taskLink(r.task))}
              >
                <Gutter cells={r.prefix} />
                <span className="blabel">
                  <span className="bname">{name}</span>
                </span>
              </div>
            )
          }
          const g = byId.get(r.id)!
          return (
            <VersionRow
              key={r.key}
              g={g}
              prefix={r.prefix}
              selected={isSel(r)}
              dragging={drag === r.id}
              picking={pick === r.id}
              onOpen={() => navigate(versionLink(r.id))}
              onDragStart={() => setDrag(r.id)}
              onDragEnd={() => {
                setDrag(null)
                setOver(null)
              }}
              onUpdate={() => graph.trunk[0] && navigate(versionLink(r.id, graph.trunk[0].commit))}
              onPick={() => navigate(`${versionLink(r.id)}?pick=${r.id}`)}
            />
          )
        })}
      </div>
      <p className="bgraph-foot">
        {pick !== null ? (
          <>
            Click the main commit to move {movingG ? <b>{versionName(movingG.attempt)}</b> : 'it'} onto.{' '}
            <button type="button" className="linkish" onClick={() => navigate(versionLink(pick))}>
              Cancel
            </button>
          </>
        ) : (
          <>Drag a version onto a main commit to rebase it. Hover for ↻ (update from main) and ⤴ (rebase onto…).</>
        )}
      </p>
    </div>
  )
}

function VersionRow({
  g,
  prefix,
  selected,
  dragging,
  picking,
  onOpen,
  onDragStart,
  onDragEnd,
  onUpdate,
  onPick,
}: {
  g: GraphAttempt
  prefix: Cell[]
  selected: boolean
  dragging: boolean
  picking: boolean
  onOpen: () => void
  onDragStart: () => void
  onDragEnd: () => void
  onUpdate: () => void
  onPick: () => void
}) {
  const a = g.attempt
  const movable = a.id !== 1 && a.status !== 'abandoned'
  const dot =
    g.conflicts > 0
      ? 'conf'
      : a.status === 'promoted'
        ? 'win'
        : a.status === 'passed'
          ? 'ok'
          : a.status === 'failed'
            ? 'bad'
            : a.status === 'abandoned'
              ? 'mute'
              : 'run'
  const behind = g.behind ?? 0
  return (
    <div
      role="treeitem"
      aria-selected={selected}
      className={`brow brow-ver ${selected ? 'sel' : ''} ${dragging || picking ? 'dragging' : ''} ${a.status === 'abandoned' ? 'closed' : ''}`}
      draggable={movable}
      onDragStart={(e) => {
        e.dataTransfer.effectAllowed = 'move'
        e.dataTransfer.setData('text/plain', `a${a.id}`)
        onDragStart()
      }}
      onDragEnd={onDragEnd}
      onClick={onOpen}
      title={`a${a.id} · ${a.actor}${a.model ? ` · ${a.model}` : ''}`}
    >
      <Gutter cells={prefix} />
      <span className={`bdot bdot-${dot}`} aria-hidden />
      <span className="blabel">
        <span className="bname">
          {a.status === 'promoted' ? '★ ' : ''}
          {versionName(a)}
        </span>
        {g.conflicts > 0 ? (
          <span className="bbadge bbadge-conf">{g.conflicts} conflict{g.conflicts === 1 ? '' : 's'}</span>
        ) : behind > 0 && a.status !== 'abandoned' ? (
          <span className="bbadge bbadge-behind">↓{behind}</span>
        ) : null}
        {g.pr && <span className="bbadge bbadge-pr">#{g.pr.number}</span>}
        {a.status === 'failed' && <span className="bbadge">fails</span>}
        {a.status === 'exploring' && <span className="bbadge">working</span>}
      </span>
      {movable && (
        <span className="bhover" onClick={(e) => e.stopPropagation()}>
          {behind > 0 && g.conflicts === 0 && (
            <button type="button" className="bicon" title="Update from main" aria-label="Update from main" onClick={onUpdate}>
              ↻
            </button>
          )}
          <button type="button" className="bicon" title="Rebase onto…" aria-label="Rebase onto" onClick={onPick}>
            ⤴
          </button>
        </span>
      )}
    </div>
  )
}
