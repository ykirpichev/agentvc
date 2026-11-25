import { Link, useNavigate } from 'react-router'
import { api } from '../api/client'
import { useLive, type ReceivedEvent } from '../api/live'
import { useResource } from '../api/useResource'
import { Empty, ErrorNote, PageHeader, Skeleton, StatusPill } from '../components/bits'
import { useListNavigation } from '../keyboard/useListNavigation'
import { actorName, timeAgo } from '../lib/format'

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
  const { recent, connection } = useLive()
  const { data, error, loading } = useResource('attempts', () => api.attempts())
  const running = (data ?? []).filter((a) => a.status === 'exploring' && a.id !== 1).reverse()

  const lastSnap = new Map<number, number>()
  for (const r of recent) {
    if (r.event.type === 'snapshot_created' && !lastSnap.has(r.event.snapshot.attempt_id)) {
      lastSnap.set(r.event.snapshot.attempt_id, r.event.snapshot.created_at)
    }
  }

  const nav = useListNavigation(running.length, {
    group: 'Live',
    noun: 'attempt',
    onOpen: (i) => running[i] && navigate(`/attempts/${running[i].id}`),
  })

  return (
    <div className="page">
      <PageHeader title="Live" eyebrow="Agents at work">
        <span className={`conn conn-${connection}`}>
          <span className="dot" />
          {connection === 'live' ? 'Streaming' : connection}
        </span>
      </PageHeader>
      {error && <ErrorNote error={error} />}
      {loading && <Skeleton rows={3} />}

      <h2 className="section-title">Running · {running.length}</h2>
      {data && running.length === 0 && <Empty title="No agents running">Exploring attempts appear here.</Empty>}
      <ul className="cards" role="listbox">
        {running.map((a, i) => (
          <li
            key={a.id}
            ref={nav.rowRef(i)}
            role="option"
            aria-selected={i === nav.selected}
            className="card"
            onMouseEnter={() => nav.setSelected(i)}
            onClick={() => navigate(`/attempts/${a.id}`)}
          >
            <div className="card-top">
              <span className="pulse" />
              <span className="row-id">a{a.id}</span>
              <span className="card-title">{actorName(a.actor)}</span>
              {a.task_id && <span className="muted">t{a.task_id}</span>}
            </div>
            {a.model && <div className="muted small">{a.model}</div>}
            {a.prompt && <div className="card-prompt">{a.prompt}</div>}
            <div className="card-foot muted small">
              started {timeAgo(a.created_at)}
              {lastSnap.has(a.id) && <> · snapshot {timeAgo(lastSnap.get(a.id)!)}</>}
            </div>
          </li>
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
    </div>
  )
}
