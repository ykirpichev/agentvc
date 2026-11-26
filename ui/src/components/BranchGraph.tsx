// The one branch tree: main's commits down the left, each window's stack of
// branches on the commit it sits on, and each task's versions off the commit,
// branch or version they are based on. Drag a version onto a main commit to
// rebase it there, or onto another version to stack it on top (the detail
// pane previews the move first); drag a branch onto a branch or onto main.

import { useMemo, useState, type DragEvent, type ReactNode } from 'react'
import { useLocation } from 'react-router'
import { commitLink, selectionFromPath, taskLink, useGo, usePick, versionLink } from '../core/links'
import { api } from '../api/client'
import type { Graph, GraphAttempt, TaskSummary } from '../api/types'
import { useResource } from '../api/useResource'
import { embedMode, thisWorkspace } from '../core/embed'
import { canMoveBranch, canMoveOnto, canStackOnto, layout, stackStale, type Cell, type GraphRow } from '../core/graph'
import { agentLine, shortSha, testsWords, versionName } from '../core/words'
import { timeAgo } from '../lib/format'
import { branchDrop, isBranchDrag, useStackEdit } from './stackEdit'
import { WindowStack } from './WorkspaceStacks'

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
  const navigate = useGo()
  const loc = useLocation()
  const sel = selectionFromPath(loc.pathname)
  const pick = usePick()
  const ws = useResource('workspaces', api.workspaces)
  const edit = useStackEdit()
  const [showClosed, setShowClosed] = useState(false)
  const [drag, setDrag] = useState<number | null>(null)
  const [over, setOver] = useState<string | null>(null)
  // Older daemons and spaces outside git have no workspaces: versions only.
  const windows = useMemo(() => (ws.error ? [] : (ws.data?.workspaces ?? [])), [ws.data, ws.error])
  const mine = ws.data && !ws.error ? thisWorkspace(ws.data)?.id : undefined
  const byWs = useMemo(() => new Map(windows.map((w) => [w.id, w])), [windows])
  const named = windows.filter((w) => w.branches.length > 0 || w.pending).length > 1
  const tips = useMemo(() => new Set(windows.flatMap((w) => w.branches.map((b) => b.tip).filter((t): t is string => !!t))), [windows])
  const { rows, onTip } = useMemo(
    () =>
      layout(graph, tasks, {
        showClosed,
        // This window first.
        windows: [...windows].sort((a, b) => Number(b.id === mine) - Number(a.id === mine)).map((w) => ({ id: w.id, base: w.base })),
        tips,
      }),
    [graph, tasks, showClosed, windows, mine, tips],
  )
  const byId = useMemo(() => new Map(graph.attempts.map((g) => [g.attempt.id, g])), [graph])
  const taskName = useMemo(() => new Map(tasks.map((t) => [t.id, t.goal])), [tasks])
  const closed = graph.attempts.filter((g) => g.attempt.status === 'abandoned').length
  const moving = drag ?? pick
  const movingG = moving === null ? undefined : byId.get(moving)
  const head = graph.trunk[0]?.commit

  const moveTo = (id: number, commit: string) => {
    setDrag(null)
    setOver(null)
    if (canMoveOnto(byId.get(id), commit)) navigate(versionLink(id, commit))
  }

  /** Stack `id` on top of version `target`: previewed on the moving version's page. */
  const stackOn = (id: number, target: number) => {
    setDrag(null)
    setOver(null)
    const t = byId.get(target)
    if (t && canStackOnto(graph, byId.get(id), t)) navigate(versionLink(id, `a${target}`))
  }

  const onDragOverVersion = (e: DragEvent, target: number) => {
    const t = byId.get(target)
    if (drag === null || !t || !canStackOnto(graph, byId.get(drag), t)) return
    e.preventDefault()
    e.dataTransfer.dropEffect = 'move'
    setOver(`a${target}`)
  }

  const onDragOver = (e: DragEvent, commit: string) => {
    // A window's branch moves onto main's newest commit.
    const branch = isBranchDrag(e) && commit === head
    if (!branch && (drag === null || !canMoveOnto(byId.get(drag), commit))) return
    e.preventDefault()
    e.dataTransfer.dropEffect = 'move'
    setOver(commit)
  }

  const onDropTrunk = (e: DragEvent, commit: string) => {
    e.preventDefault()
    const b = isBranchDrag(e) ? branchDrop(e) : null
    if (b) {
      setOver(null)
      const w = byWs.get(b.workspace)
      if (w && commit === head && canMoveBranch(w, b.branch, w.trunk)) {
        void edit.run(w, { op: 'move', branch: b.branch, onto: w.trunk }, `Moved ${b.branch} onto ${w.trunk.replace(/^origin\//, '')}.`, true)
      }
      return
    }
    if (drag !== null) moveTo(drag, commit)
  }

  const isSel = (r: GraphRow) =>
    (r.kind === 'version' && sel?.kind === 'version' && sel.id === r.id) ||
    (r.kind === 'task' && sel?.kind === 'task' && sel.id === r.task) ||
    (r.kind === 'trunk' && sel?.kind === 'commit' && r.commit.startsWith(sel.commit))

  const renderRow = (r: GraphRow): ReactNode => {
    if (r.kind === 'window') {
      const w = byWs.get(r.workspace)
      if (!w) return null
      return (
        <div key={r.key} className={`wst-in ${r.lead === 'trunk' ? 'lead-trunk' : ''}`}>
          <WindowStack
            w={w}
            mine={w.id === mine}
            showHead={named}
            under={(b) => {
              const sub = b.tip ? onTip.get(b.tip) : undefined
              return sub ? (
                <div className="wst-under" style={{ paddingLeft: `${24 + Math.max(0, b.depth) * 14 + 14}px` }}>
                  {sub.map(renderRow)}
                </div>
              ) : null
            }}
          />
        </div>
      )
    }

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
            onDrop={(e) => onDropTrunk(e, r.commit)}
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
      const stackTarget = movingG !== undefined && canStackOnto(graph, movingG, g)
      const stale = stackStale(graph, g)
      return (
        <VersionRow
          key={r.key}
          g={g}
          stale={stale ? versionName(stale.attempt) : null}
          onBranch={!!g.base_commit && tips.has(g.base_commit)}
          taskLabel={
            g.stacked_on && byId.get(g.stacked_on.attempt)?.attempt.task_id !== g.attempt.task_id && g.attempt.task_id !== null
              ? (taskName.get(g.attempt.task_id) ?? `Task ${g.attempt.task_id}`)
              : null
          }
          drop={over === `a${r.id}`}
          target={pick !== null && stackTarget}
          onDragOver={(e) => onDragOverVersion(e, r.id)}
          onDragLeave={() => setOver((o) => (o === `a${r.id}` ? null : o))}
          onDrop={(e) => {
            e.preventDefault()
            if (drag !== null) stackOn(drag, r.id)
          }}
          prefix={r.prefix}
          selected={isSel(r)}
          dragging={drag === r.id}
          picking={pick === r.id}
          onOpen={() => (pick !== null && stackTarget ? stackOn(pick, r.id) : navigate(versionLink(r.id)))}
          onDragStart={() => setDrag(r.id)}
          onDragEnd={() => {
            setDrag(null)
            setOver(null)
          }}
          onUpdate={() =>
            g.stacked_on
              ? navigate(versionLink(r.id, `a${g.stacked_on.attempt}`))
              : graph.trunk[0] && navigate(versionLink(r.id, graph.trunk[0].commit))
          }
          onPick={() => navigate(`${versionLink(r.id)}?pick=${r.id}`)}
        />
      )
      }

  return (
    <div className="bgraph">
      {embedMode !== 'sidebar' && <div className="bgraph-head">Branches</div>}
      <div className="bgraph-rows" role="tree" aria-label="Branches">
        {rows.map(renderRow)}
      </div>
      {pick !== null ? (
        <p className="bgraph-foot">
          Click a main commit to move {movingG ? <b>{versionName(movingG.attempt)}</b> : 'it'} onto, or a version to stack it on.{' '}
          <button type="button" className="linkish" onClick={() => navigate(versionLink(pick))}>
            Cancel
          </button>
        </p>
      ) : (
        closed > 0 && (
          <p className="bgraph-foot">
            <button type="button" className="linkish" onClick={() => setShowClosed((s) => !s)}>
              {showClosed ? 'Hide' : 'Show'} {closed} not chosen
            </button>
          </p>
        )
      )}
    </div>
  )
}

function VersionRow({
  g,
  stale,
  onBranch,
  taskLabel,
  drop,
  target,
  onDragOver,
  onDragLeave,
  onDrop,
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
  /** Name of the version it is stacked on, when that one moved on since. */
  stale: string | null
  /** Based on a window's branch (it hangs under it). */
  onBranch: boolean
  /** The task's name, when the row sits under another task's version. */
  taskLabel: string | null
  drop: boolean
  target: boolean
  onDragOver: (e: DragEvent) => void
  onDragLeave: () => void
  onDrop: (e: DragEvent) => void
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
  // A stacked version updates from the version below it, and one based on a
  // window's branch moves with that branch, not from main.
  const behind = g.stacked_on || onBranch ? 0 : (g.behind ?? 0)
  const canUpdate = (behind > 0 || stale !== null) && g.conflicts === 0
  return (
    <div
      role="treeitem"
      aria-selected={selected}
      className={`brow brow-ver ${selected ? 'sel' : ''} ${dragging || picking ? 'dragging' : ''} ${a.status === 'abandoned' ? 'closed' : ''} ${drop ? 'drop' : ''} ${target ? 'target' : ''}`}
      draggable={movable}
      onDragStart={(e) => {
        e.dataTransfer.effectAllowed = 'move'
        e.dataTransfer.setData('text/plain', `a${a.id}`)
        onDragStart()
      }}
      onDragEnd={onDragEnd}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
      onClick={onOpen}
      title={[
        `a${a.id} · ${a.actor}${a.model ? ` · ${a.model}` : ''}`,
        g.tests ? testsWords(g.tests).label : '',
        agentLine(g.agent, a.model),
      ]
        .filter(Boolean)
        .join('\n')}
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
        ) : stale !== null && a.status !== 'abandoned' ? (
          <span className="bbadge bbadge-behind" title={`${stale} changed since this was stacked on it`}>
            ↓
          </span>
        ) : null}
        {g.pr && <span className="bbadge bbadge-pr">#{g.pr.number}</span>}
        {(a.status === 'failed' || (a.status === 'promoted' && g.tests && !g.tests.passed && !g.tests.stale)) && (
          <span className="bbadge">fails</span>
        )}
        {a.status === 'exploring' && <span className="bbadge">working</span>}
        {taskLabel && <span className="bsub btask">{taskLabel}</span>}
      </span>
      {movable && (
        <span className="bhover" onClick={(e) => e.stopPropagation()}>
          {canUpdate && (
            <button
              type="button"
              className="bicon"
              title={stale !== null ? `Update from ${stale}` : 'Update from main'}
              aria-label={stale !== null ? `Update from ${stale}` : 'Update from main'}
              onClick={onUpdate}
            >
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
