import { useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router'
import { api, ref } from '../api/client'
import type { Attempt, AttemptStats, Snapshot } from '../api/types'
import { useResource } from '../api/useResource'
import { AttemptMeta } from '../components/AttemptMeta'
import { ChangeList } from '../components/ChangeList'
import { Empty, ErrorNote, Kbd, PageHeader, Skeleton, StatusPill } from '../components/bits'
import { Icon } from '../components/Icon'
import { ActionButtons, useAttemptActions } from '../components/ReviewActions'
import { StatChips } from '../components/StatChips'
import { useHotkeys } from '../keyboard/hotkeys'
import { useListNavigation } from '../keyboard/useListNavigation'
import { findLink } from '../lib/find'
import { actorName, fullDate, money, timeAgo } from '../lib/format'
import { triggerKind, triggerLabel } from '../lib/snapshot'
import { visibleTree, type VisibleRow } from '../lib/tree'

const MAX_COMPARE = 4

/** Rank for "compare the best": passed first, then failed, then running. */
const RANK: Record<string, number> = { passed: 0, failed: 1, exploring: 2 }

export function TaskPage() {
  const id = Number(useParams().id)
  const navigate = useNavigate()
  const { data, error, loading } = useResource(`task:${id}`, () => api.task(id))
  const stats = useResource(`task-stats:${id}`, () => api.taskStats(id))
  const statsById = new Map((stats.data ?? []).map((s) => [s.attempt, s]))
  const [expanded, setExpanded] = useState<Set<number>>(new Set())
  const [checked, setChecked] = useState<number[]>([])
  const attempts = data?.attempts ?? []
  const rows = visibleTree(attempts, expanded)

  const open = (i: number) => {
    const r = rows[i]
    if (r) navigate(`/attempts/${r.attempt.id}`)
  }
  const nav = useListNavigation(rows.length, { onOpen: open, group: 'Attempts', noun: 'attempt' })
  const current = rows[nav.selected]?.attempt
  const actions = useAttemptActions(current, attempts, { group: 'Review' })

  const live = checked.filter((c) => attempts.some((a) => a.id === c))
  const toggle = (aid: number) =>
    setChecked((xs) =>
      xs.includes(aid) ? xs.filter((x) => x !== aid) : xs.length >= MAX_COMPARE ? xs : [...xs, aid],
    )
  const toggleFold = (aid: number) =>
    setExpanded((s) => {
      const n = new Set(s)
      if (n.has(aid)) n.delete(aid)
      else n.add(aid)
      return n
    })
  const bestOpen = attempts
    .filter((a) => a.status in RANK)
    .sort((a, b) => RANK[a.status]! - RANK[b.status]! || a.id - b.id)
    .slice(0, MAX_COMPARE)
    .map((a) => a.id)
  const compare = (ids: number[]) => {
    if (ids.length >= 2) navigate(`/compare?a=${[...ids].sort((x, y) => x - y).join(',')}`)
  }

  useHotkeys([
    { keys: ['u', 'backspace'], description: 'Back to tasks', group: 'Task', handler: () => navigate('/tasks') },
    {
      keys: 'space',
      description: 'Select for compare',
      group: 'Compare',
      handler: () => current && toggle(current.id),
    },
    {
      keys: 'c',
      description: 'Compare selected (2 to 4)',
      group: 'Compare',
      handler: () => compare(live.length >= 2 ? live : bestOpen),
    },
    {
      keys: 'esc',
      description: 'Clear selection',
      group: 'Compare',
      handler: () => setChecked([]),
    },
    {
      keys: 'e',
      description: 'Fold or unfold abandoned branch',
      group: 'Attempts',
      handler: () => current && toggleFold(current.id),
    },
  ])

  return (
    <div className="page page-wide">
      <Link to="/tasks" className="back">
        <Icon name="back" size={14} /> Tasks
      </Link>
      {error && <ErrorNote error={error} />}
      {loading && <Skeleton />}
      {data && (
        <>
          <PageHeader title={data.task.goal} eyebrow={`t${data.task.id} · created ${timeAgo(data.task.created_at)}`}>
            <TaskTotals attempts={attempts} stats={stats.data} />
            <Link className="btn btn-ghost btn-sm" to={findLink({ task: data.task.id })} title="Filter this task's attempts">
              <Icon name="search" size={13} /> Find
            </Link>
          </PageHeader>
          {data.task.criteria && (
            <div className="criteria">
              <Icon name="check" size={14} />
              <span>
                <span className="label">Done when</span> {data.task.criteria}
              </span>
            </div>
          )}
          {rows.length === 0 ? (
            <Empty title="No attempts yet">
              Start one with <code>agentvc fork --task t{data.task.id}</code>.
            </Empty>
          ) : (
            <div className="split">
              <section className="panel tree-panel">
                <div className="panel-head">
                  <span>Attempt tree</span>
                  <span className="muted">{attempts.length}</span>
                </div>
                <ul className="tree" role="listbox" aria-multiselectable="true">
                  {rows.map((r, i) => (
                    <TreeItem
                      key={r.attempt.id}
                      row={r}
                      stats={statsById.get(r.attempt.id)}
                      selected={i === nav.selected}
                      checked={live.includes(r.attempt.id)}
                      rowRef={nav.rowRef(i)}
                      onFocus={() => nav.setSelected(i)}
                      onOpen={() => open(i)}
                      onCheck={() => toggle(r.attempt.id)}
                      onFold={() => toggleFold(r.attempt.id)}
                      unfolded={expanded.has(r.attempt.id)}
                    />
                  ))}
                </ul>
                <div className={`compare-bar ${live.length > 0 ? 'on' : ''}`}>
                  {live.length === 0 ? (
                    <>
                      <span className="muted">
                        <Kbd keys="space" /> select · <Kbd keys="c" /> compare
                      </span>
                      {bestOpen.length >= 2 && (
                        <button className="btn btn-ghost btn-sm" onClick={() => compare(bestOpen)}>
                          <Icon name="compare" size={13} /> Compare best {bestOpen.length}
                        </button>
                      )}
                    </>
                  ) : (
                    <>
                      <span>
                        {live.map((c) => `a${c}`).join(' · ')}
                        <span className="muted"> {live.length < 2 ? '· pick one more' : ''}</span>
                      </span>
                      <span className="compare-bar-actions">
                        <button className="btn btn-ghost btn-sm" onClick={() => setChecked([])}>
                          Clear
                        </button>
                        <button className="btn btn-sm" disabled={live.length < 2} onClick={() => compare(live)}>
                          <Icon name="compare" size={13} /> Compare {live.length} <Kbd keys="c" />
                        </button>
                      </span>
                    </>
                  )}
                </div>
              </section>
              {current && (
                <AttemptPreview
                  key={current.id}
                  attempt={current}
                  stats={statsById.get(current.id)}
                  actions={<ActionButtons actions={actions} />}
                />
              )}
            </div>
          )}
        </>
      )}
      {actions.node}
    </div>
  )
}

function TaskTotals({ attempts, stats }: { attempts: Attempt[]; stats: AttemptStats[] | undefined }) {
  const cost = (stats ?? []).reduce((s, x) => s + (x.cost_usd ?? 0), 0)
  const winner = attempts.find((a) => a.status === 'promoted')
  return (
    <span className="task-totals">
      {winner ? (
        <Link to={`/attempts/${winner.id}`} className="winner-chip">
          <Icon name="star" size={13} /> Winner a{winner.id}
        </Link>
      ) : (
        <span className="muted">No winner yet</span>
      )}
      {cost > 0 && (
        <span className="stat-chip">
          <Icon name="coin" size={13} /> {money(cost)} total
        </span>
      )}
    </span>
  )
}

function TreeItem({
  row,
  stats,
  selected,
  checked,
  rowRef,
  onFocus,
  onOpen,
  onCheck,
  onFold,
  unfolded,
}: {
  row: VisibleRow
  stats: AttemptStats | undefined
  selected: boolean
  checked: boolean
  rowRef: (el: HTMLElement | null) => void
  onFocus: () => void
  onOpen: () => void
  onCheck: () => void
  onFold: () => void
  unfolded: boolean
}) {
  const a = row.attempt
  const abandoned = a.status === 'abandoned'
  const title = abandoned ? `Abandoned${a.reason ? `: ${a.reason}` : ''}` : a.reason ?? undefined
  const check = stats?.check
  return (
    <li
      ref={rowRef}
      role="option"
      aria-selected={selected}
      aria-checked={checked}
      title={title}
      className={[
        'tree-row',
        `status-${a.status}`,
        row.onWinningPath ? 'on-path' : '',
        abandoned ? 'folded-row' : '',
        checked ? 'checked' : '',
      ].join(' ')}
      style={{ '--depth': row.depth } as React.CSSProperties}
      onMouseEnter={onFocus}
      onClick={onFocus}
      onDoubleClick={onOpen}
    >
      <span className="tree-guide" />
      <button
        className="tree-check"
        aria-label={checked ? `Unselect a${a.id}` : `Select a${a.id} for compare`}
        onClick={(e) => {
          e.stopPropagation()
          onCheck()
        }}
      >
        {checked && <Icon name="check" size={11} />}
      </button>
      <span className="node" />
      <span className="row-id">a{a.id}</span>
      <span className="tree-label">
        {actorName(a.actor)}
        {a.model && !abandoned && <span className="muted"> · {a.model}</span>}
        {abandoned && a.reason && <span className="tree-reason"> · {a.reason}</span>}
      </span>
      {a.tags?.map((t) => (
        <span key={t} className="tag-chip" title={`agentvc tag ${t}`}>
          #{t}
        </span>
      ))}
      {row.root && a.parent_attempt && a.parent_attempt !== 1 && <span className="muted tree-from">from a{a.parent_attempt}</span>}
      {(row.folded > 0 || unfolded) && (
        <button
          className="fold-chip"
          onClick={(e) => {
            e.stopPropagation()
            onFold()
          }}
          title={unfolded ? 'Fold this abandoned branch (e)' : 'Show the abandoned branch (e)'}
        >
          <Icon name={unfolded ? 'minus' : 'plus'} size={10} />
          {row.folded > 0 ? row.folded : ''}
        </button>
      )}
      {check && !abandoned && (
        <span className={`check-dot ${check.passed ? 'ok' : 'bad'}`} title={check.passed ? 'Checks passed' : 'Checks failed'}>
          <Icon name="flask" size={12} />
        </span>
      )}
      {abandoned ? <span className="pill-mini">abandoned</span> : <StatusPill status={a.status} />}
    </li>
  )
}

function AttemptPreview({
  attempt,
  stats,
  actions,
}: {
  attempt: Attempt
  stats: AttemptStats | undefined
  actions: React.ReactNode
}) {
  const navigate = useNavigate()
  const diff = useResource(`diff:${attempt.id}`, () => api.diff(ref.attempt(attempt.id)))
  const snaps = useResource(`snapshots:${attempt.id}`, () => api.snapshots(attempt.id))
  return (
    <section className="panel preview">
      <div className="preview-head">
        <h2>
          <span className="row-id">a{attempt.id}</span> {actorName(attempt.actor)}
        </h2>
        <StatusPill status={attempt.status} />
      </div>
      <div className="preview-actions">
        {actions}
        <Link className="btn btn-ghost" to={`/attempts/${attempt.id}`}>
          Review changes <Kbd keys="enter" />
        </Link>
      </div>
      {stats?.summary && <p className="agent-summary">{stats.summary}</p>}
      {stats && <StatChips stats={stats} className="preview-stats" />}
      <AttemptMeta
        attempt={attempt}
        extra={
          stats?.cost_usd != null ? (
            <div>
              <dt>Cost</dt>
              <dd>{money(stats.cost_usd)}</dd>
            </div>
          ) : undefined
        }
      />
      <div className="preview-cols">
        <div>
          <div className="panel-head">
            <span>Changed files</span>
            <span className="muted">{diff.data?.changes.length ?? ''}</span>
          </div>
          {diff.error && <ErrorNote error={diff.error} />}
          {diff.data && diff.data.changes.length === 0 && (
            <div className="diff-note">No changes from the fork point yet.</div>
          )}
          {diff.data && (
            <ChangeList
              changes={diff.data.changes}
              onSelect={(i) =>
                navigate(`/attempts/${attempt.id}?file=${encodeURIComponent(diff.data!.changes[i]!.path)}`)
              }
            />
          )}
        </div>
        <div>
          <div className="panel-head">
            <span>Snapshots</span>
            <span className="muted">{snaps.data?.length ?? ''}</span>
          </div>
          {snaps.data && <Timeline attemptId={attempt.id} snapshots={snaps.data} />}
        </div>
      </div>
    </section>
  )
}

const TIMELINE_MAX = 12

/** Newest first; the trigger says what caused each snapshot. */
function Timeline({ attemptId, snapshots }: { attemptId: number; snapshots: Snapshot[] }) {
  const [all, setAll] = useState(false)
  const list = [...snapshots].reverse()
  const shown = all ? list : list.slice(0, TIMELINE_MAX)
  if (list.length === 0) return <div className="diff-note">No snapshots yet.</div>
  return (
    <ol className="timeline">
      {shown.map((s) => (
        <li key={s.id} className={`tl-item tl-${triggerKind(s.trigger)}`}>
          <span className="tl-dot" />
          <Link to={`/attempts/${attemptId}?snap=${s.id}`} className="tl-main">
            <span className="tl-trigger">{triggerLabel(s)}</span>
            <span className="row-id muted">s{s.id}</span>
          </Link>
          <span className="tl-time muted" title={fullDate(s.created_at)}>
            {timeAgo(s.created_at)}
          </span>
        </li>
      ))}
      {list.length > TIMELINE_MAX && !all && (
        <li className="tl-more">
          <button className="link-btn" onClick={() => setAll(true)}>
            Show {list.length - TIMELINE_MAX} older
          </button>
        </li>
      )}
    </ol>
  )
}
