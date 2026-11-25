import { useEffect, useState } from 'react'
import { Link, useNavigate } from 'react-router'
import { api, ApiError } from '../api/client'
import { useLive, type ReceivedEvent } from '../api/live'
import type { LiveAttempt } from '../api/types'
import { useResource } from '../api/useResource'
import { Empty, ErrorNote, Kbd, PageHeader, Skeleton, StatusPill } from '../components/bits'
import { Icon } from '../components/Icon'
import { useAttemptActions } from '../components/ReviewActions'
import { useHotkeys } from '../keyboard/hotkeys'
import { useListNavigation } from '../keyboard/useListNavigation'
import { actorName, duration, money, plural, timeAgo } from '../lib/format'
import { triggerLabel } from '../lib/snapshot'

function describe(e: ReceivedEvent['event']): { text: React.ReactNode; to: string } {
  switch (e.type) {
    case 'task_created':
      return { text: <>New task <b>t{e.task.id}</b> {e.task.goal}</>, to: `/tasks/${e.task.id}` }
    case 'attempt_created':
      return {
        text: (
          <>
            <b>{actorName(e.attempt.actor)}</b> started <b>a{e.attempt.id}</b>
            {e.attempt.task_id && <> on t{e.attempt.task_id}</>}
          </>
        ),
        to: `/attempts/${e.attempt.id}`,
      }
    case 'attempt_updated':
      return {
        text: (
          <>
            <b>a{e.attempt.id}</b> {e.previous_status} → <StatusPill status={e.attempt.status} />
            {e.attempt.reason && <span className="muted"> {e.attempt.reason}</span>}
          </>
        ),
        to: `/attempts/${e.attempt.id}`,
      }
    case 'snapshot_created':
      return {
        text: (
          <>
            Snapshot <b>s{e.snapshot.id}</b> of a{e.snapshot.attempt_id}{' '}
            <span className="muted">({e.snapshot.trigger})</span>
          </>
        ),
        to: `/attempts/${e.snapshot.attempt_id}?snap=${e.snapshot.id}`,
      }
  }
}

export function Live() {
  const navigate = useNavigate()
  const { recent, connection, refresh } = useLive()
  const { data, error, loading } = useResource('live', api.live)
  const running = data ?? []
  const all = useResource('attempts', () => api.attempts())
  const now = useNow(15_000)

  // Selection follows the attempt, not the position, so a card that starts
  // or finishes elsewhere on the board never moves the cursor to another agent.
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const selectedIdx = Math.max(0, running.findIndex((r) => r.attempt.id === selectedId))
  const nav = useListNavigation(running.length, {
    group: 'Live',
    noun: 'agent',
    selected: selectedIdx,
    onSelect: (i) => setSelectedId(running[i]?.attempt.id ?? null),
    onOpen: (i) => running[i] && navigate(`/attempts/${running[i].attempt.id}`),
  })
  const focused = running[nav.selected]
  const actions = useAttemptActions(
    focused?.attempt,
    (all.data ?? []).filter((a) => a.task_id !== null && a.task_id === focused?.attempt.task_id),
    { group: 'Focused agent' },
  )

  // Stopping kills the agent's process, so it takes two presses.
  const [armed, setArmed] = useState<number | null>(null)
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null)
  useEffect(() => {
    if (armed === null) return
    const t = window.setTimeout(() => setArmed(null), 3000)
    return () => window.clearTimeout(t)
  }, [armed])
  const stop = async (la: LiveAttempt) => {
    const id = la.attempt.id
    if (armed !== id) return setArmed(id)
    setArmed(null)
    try {
      const r = await api.stop(id)
      setNote({ ok: true, text: `Sent SIG${r.signal} to a${id} (pid ${r.pid})` })
    } catch (e) {
      setNote({ ok: false, text: e instanceof ApiError ? `${e.code}: ${e.message}` : String(e) })
    }
    refresh()
  }
  useHotkeys([
    {
      keys: 's',
      description: 'Stop the agent (press twice)',
      group: 'Focused agent',
      handler: () => focused?.run?.alive && void stop(focused),
    },
  ])

  const overlapCount = running.filter((r) => Object.keys(r.overlaps).length > 0).length
  const spend = running.reduce((t, r) => t + (r.stats.cost_usd ?? 0), 0)

  return (
    <div className="page page-wide">
      <PageHeader title="Live" eyebrow="Agents at work">
        <span className="live-totals">
          {overlapCount > 0 && (
            <span className="stat-chip chip-warn" title="Running attempts that changed the same files">
              <Icon name="warn" size={13} /> {plural(overlapCount, 'agent')} overlapping
            </span>
          )}
          {spend > 0 && (
            <span className="stat-chip">
              <Icon name="coin" size={13} /> {money(spend)} so far
            </span>
          )}
          <span className={`conn conn-${connection}`}>
            <span className="dot" />
            {connection === 'live' ? 'Streaming' : connection}
          </span>
        </span>
      </PageHeader>
      {error && <ErrorNote error={error} />}
      {loading && <Skeleton rows={3} />}
      {note && (
        <div className={note.ok ? 'agree-note' : 'error-note'} role="status" onClick={() => setNote(null)}>
          {note.text}
        </div>
      )}

      <h2 className="section-title">
        Running · {running.length}
        {running.length > 0 && (
          <span className="muted section-keys">
            <Kbd keys="j" /> <Kbd keys="k" /> move · <Kbd keys="enter" /> open · <Kbd keys="s" /> stop ·{' '}
            <Kbd keys="x" /> abandon
          </span>
        )}
      </h2>
      {data && running.length === 0 && (
        <Empty title="No agents running">
          Start one with <code>agentvc run --task tN -- your-agent</code>; it shows up here.
        </Empty>
      )}
      <ul className="live-board" role="listbox">
        {running.map((la, i) => (
          <LiveCard
            key={la.attempt.id}
            la={la}
            now={now}
            selected={i === nav.selected}
            armed={armed === la.attempt.id}
            rowRef={nav.rowRef(i)}
            onFocus={() => nav.setSelected(i)}
            onOpen={() => navigate(`/attempts/${la.attempt.id}`)}
            onStop={() => void stop(la)}
            onAbandon={() => {
              nav.setSelected(i)
              actions.abandon()
            }}
          />
        ))}
      </ul>

      <h2 className="section-title">Activity</h2>
      {recent.length === 0 ? (
        <div className="diff-note">Events from agents and the CLI stream in here as they happen.</div>
      ) : (
        <ol className="feed">
          {recent.slice(0, 100).map((r) => {
            const d = describe(r.event)
            return (
              <li key={r.seq}>
                <time>{new Date(r.at).toLocaleTimeString()}</time>
                <Link to={d.to}>{d.text}</Link>
              </li>
            )
          })}
        </ol>
      )}
      {actions.node}
    </div>
  )
}

/** Current time in seconds, refreshed every `ms` so "3m ago" stays true. */
function useNow(ms: number): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), ms)
    return () => window.clearInterval(t)
  }, [ms])
  return now
}

function LiveCard({
  la,
  now,
  selected,
  armed,
  rowRef,
  onFocus,
  onOpen,
  onStop,
  onAbandon,
}: {
  la: LiveAttempt
  now: number
  selected: boolean
  armed: boolean
  rowRef: (el: HTMLElement | null) => void
  onFocus: () => void
  onOpen: () => void
  onStop: () => void
  onAbandon: () => void
}) {
  const a = la.attempt
  const run = la.run
  const overlapping = Object.keys(la.overlaps).length
  const lastAt = la.head?.created_at ?? a.created_at
  const idle = now / 1000 - lastAt > 10 * 60
  return (
    <li
      ref={rowRef}
      role="option"
      aria-selected={selected}
      className={`live-card ${idle ? 'idle' : ''}`}
      onMouseEnter={onFocus}
      onDoubleClick={onOpen}
    >
      <div className="card-top">
        <span className={idle ? 'idle-dot' : 'pulse'} title={idle ? 'No snapshot for 10 minutes' : 'Active'} />
        <Link to={`/attempts/${a.id}`} className="row-id ref">
          a{a.id}
        </Link>
        <span className="card-title">
          {actorName(a.actor)}
          {a.model && <span className="muted"> · {a.model}</span>}
        </span>
        <span className="muted small" title="Since the attempt started">
          {duration(now / 1000 - a.created_at)}
        </span>
      </div>
      {a.task_id && (
        <Link to={`/tasks/${a.task_id}`} className="live-task">
          t{a.task_id} · {la.task_goal}
        </Link>
      )}

      <div className="live-step">
        <span className="label">Now</span>
        <span className="live-step-text">{la.head ? triggerLabel(la.head) : 'Starting'}</span>
        <span className="muted small">{timeAgo(lastAt, now)}</span>
      </div>

      <div className="stat-chips">
        <span className="stat-chip">
          <Icon name="coin" size={13} />
          {la.stats.cost_usd != null ? money(la.stats.cost_usd) : <span className="muted">no cost data</span>}
        </span>
        <span className="stat-chip muted" title="Snapshots recorded">
          <Icon name="camera" size={13} />
          {la.stats.snapshots}
        </span>
        <span className={`stat-chip ${overlapping ? 'chip-warn' : ''}`}>
          <Icon name="file" size={13} />
          {plural(la.files_changed, 'file')}
          {overlapping > 0 && <> · {overlapping} shared</>}
        </span>
      </div>

      {la.files.length > 0 && (
        <ul className="live-files">
          {la.files.map((f) => {
            const others = la.overlaps[f.path]
            return (
              <li key={f.path} className={others ? 'overlap' : ''} title={f.path}>
                <span className={`change-sign change-${f.kind}`}>{f.kind[0]!.toUpperCase()}</span>
                <span className="change-path">{f.path}</span>
                {others && (
                  <span className="overlap-with" title="Also changed by these running attempts">
                    <Icon name="warn" size={11} /> {others.map((o) => `a${o}`).join(', ')}
                  </span>
                )}
              </li>
            )
          })}
          {la.files_changed > la.files.length && (
            <li className="muted small">and {la.files_changed - la.files.length} more</li>
          )}
        </ul>
      )}

      {run && (
        <div className={`live-run ${run.alive ? 'alive' : 'dead'}`}>
          <div className="live-run-head">
            <span className={`run-state ${run.alive ? 'on' : 'off'}`}>{run.alive ? 'running' : 'process gone'}</span>
            <code className="live-cmd" title={run.command.join(' ')}>
              {run.command.join(' ')}
            </code>
            <span className="muted small">r{run.id}</span>
          </div>
          {run.check_command && (
            <div className="muted small">
              then check <code>{run.check_command}</code>
            </div>
          )}
          {run.log_tail.length > 0 && <pre className="live-log">{run.log_tail.join('\n')}</pre>}
        </div>
      )}

      <div className="live-actions">
        {run?.alive && (
          <button className={`btn btn-sm ${armed ? 'btn-danger' : 'btn-ghost'}`} onClick={onStop}>
            {armed ? 'Press again to stop' : 'Stop'} <Kbd keys="s" />
          </button>
        )}
        <button className="btn btn-sm btn-ghost" onClick={onAbandon}>
          Abandon <Kbd keys="x" />
        </button>
        <Link className="btn btn-sm btn-ghost" to={`/attempts/${a.id}`}>
          Changes <Kbd keys="enter" />
        </Link>
      </div>
    </li>
  )
}
