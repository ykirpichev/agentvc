import { Link, useNavigate, useParams } from 'react-router'
import { api, ref } from '../api/client'
import type { Attempt } from '../api/types'
import { useResource } from '../api/useResource'
import { AttemptMeta } from '../components/AttemptMeta'
import { ChangeList } from '../components/ChangeList'
import { Empty, ErrorNote, Kbd, PageHeader, Skeleton, StatusPill } from '../components/bits'
import { Icon } from '../components/Icon'
import { useHotkeys } from '../keyboard/hotkeys'
import { useListNavigation } from '../keyboard/useListNavigation'
import { actorName, timeAgo } from '../lib/format'
import { attemptTree } from '../lib/tree'

export function TaskPage() {
  const id = Number(useParams().id)
  const navigate = useNavigate()
  const { data, error, loading } = useResource(`task:${id}`, () => api.task(id))
  const rows = data ? attemptTree(data.attempts) : []
  const open = (i: number) => {
    const r = rows[i]
    if (r) navigate(`/attempts/${r.attempt.id}`)
  }
  const nav = useListNavigation(rows.length, { onOpen: open, group: 'Attempts', noun: 'attempt' })
  useHotkeys([
    { keys: ['u', 'backspace'], description: 'Back to tasks', group: 'Task', handler: () => navigate('/tasks') },
  ])
  const current = rows[nav.selected]?.attempt

  return (
    <div className="page page-wide">
      <Link to="/tasks" className="back">
        <Icon name="back" size={14} /> Tasks
      </Link>
      {error && <ErrorNote error={error} />}
      {loading && <Skeleton />}
      {data && (
        <>
          <PageHeader title={data.task.goal} eyebrow={`t${data.task.id} · created ${timeAgo(data.task.created_at)}`} />
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
              <section className="panel">
                <div className="panel-head">
                  <span>Attempt tree</span>
                  <span className="muted">{rows.length}</span>
                </div>
                <ul className="tree" role="listbox">
                  {rows.map((r, i) => (
                    <li
                      key={r.attempt.id}
                      ref={nav.rowRef(i)}
                      role="option"
                      aria-selected={i === nav.selected}
                      className={`tree-row status-${r.attempt.status}`}
                      style={{ '--depth': r.depth } as React.CSSProperties}
                      onMouseEnter={() => nav.setSelected(i)}
                      onClick={() => nav.setSelected(i)}
                      onDoubleClick={() => open(i)}
                    >
                      <span className="tree-guide" />
                      <span className="node" />
                      <span className="row-id">a{r.attempt.id}</span>
                      <span className="tree-label">
                        {actorName(r.attempt.actor)}
                        {r.attempt.model && <span className="muted"> · {r.attempt.model}</span>}
                      </span>
                      {r.root && r.attempt.parent_attempt && (
                        <span className="muted tree-from">from a{r.attempt.parent_attempt}</span>
                      )}
                      <StatusPill status={r.attempt.status} />
                    </li>
                  ))}
                </ul>
                <div className="panel-foot muted">
                  <Kbd keys="j" /> <Kbd keys="k" /> select · <Kbd keys="enter" /> open
                </div>
              </section>
              {current && <AttemptPreview key={current.id} attempt={current} />}
            </div>
          )}
        </>
      )}
    </div>
  )
}

function AttemptPreview({ attempt }: { attempt: Attempt }) {
  const navigate = useNavigate()
  const diff = useResource(`diff:${attempt.id}`, () => api.diff(ref.attempt(attempt.id)))
  return (
    <section className="panel preview">
      <div className="preview-head">
        <h2>
          <span className="row-id">a{attempt.id}</span> {actorName(attempt.actor)}
        </h2>
        <StatusPill status={attempt.status} />
        <Link className="btn" to={`/attempts/${attempt.id}`}>
          Review changes <Kbd keys="enter" />
        </Link>
      </div>
      <AttemptMeta attempt={attempt} />
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
    </section>
  )
}
