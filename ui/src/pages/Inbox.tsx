import { api } from '../api/client'
import { useResource } from '../api/useResource'
import { Empty, ErrorNote, Kbd, PageHeader, Skeleton } from '../components/bits'
import { TaskList } from '../components/TaskList'
import { needsDecision } from '../lib/inbox'

export function Inbox() {
  const { data, error, loading } = useResource('tasks', api.tasks)
  const waiting = (data ?? [])
    .filter(needsDecision)
    // Tasks with a passing attempt first: those are ready to pick.
    .sort((a, b) => Number(b.attempts.passed > 0) - Number(a.attempts.passed > 0) || b.updated_at - a.updated_at)

  return (
    <div className="page">
      <PageHeader title="Inbox" eyebrow="Waiting on you">
        <span className="muted">
          <Kbd keys="j" /> <Kbd keys="k" /> to move, <Kbd keys="enter" /> to open
        </span>
      </PageHeader>
      {error && <ErrorNote error={error} />}
      {loading && <Skeleton />}
      {data && waiting.length === 0 && (
        <Empty title="Inbox zero">
          Tasks show up here when an attempt passes or fails and no winner has been picked.
        </Empty>
      )}
      {waiting.length > 0 && <TaskList tasks={waiting} group="Inbox" />}
    </div>
  )
}
