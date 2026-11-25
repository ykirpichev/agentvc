import { useState } from 'react'
import { api } from '../api/client'
import type { TaskSummary } from '../api/types'
import { useResource } from '../api/useResource'
import { Empty, ErrorNote, PageHeader, Skeleton } from '../components/bits'
import { TaskList } from '../components/TaskList'
import { useHotkeys } from '../keyboard/hotkeys'

type Filter = 'open' | 'done' | 'all'
const FILTERS: { id: Filter; label: string; test: (t: TaskSummary) => boolean }[] = [
  { id: 'open', label: 'Open', test: (t) => t.attempts.promoted === 0 },
  { id: 'done', label: 'Done', test: (t) => t.attempts.promoted > 0 },
  { id: 'all', label: 'All', test: () => true },
]

export function Tasks() {
  const { data, error, loading } = useResource('tasks', api.tasks)
  const [filter, setFilter] = useState<Filter>('open')
  const f = FILTERS.find((x) => x.id === filter)!
  const tasks = (data ?? []).filter(f.test).sort((a, b) => b.updated_at - a.updated_at)

  useHotkeys(
    FILTERS.map((x, i) => ({
      keys: String(i + 1),
      description: `Show ${x.label.toLowerCase()} tasks`,
      group: 'Tasks',
      handler: () => setFilter(x.id),
    })),
  )

  return (
    <div className="page">
      <PageHeader title="Tasks" eyebrow={data ? `${data.length} in this space` : undefined}>
        <div className="segmented" role="tablist">
          {FILTERS.map((x) => (
            <button
              key={x.id}
              role="tab"
              aria-selected={filter === x.id}
              onClick={() => setFilter(x.id)}
            >
              {x.label}
              <span className="seg-count">{(data ?? []).filter(x.test).length}</span>
            </button>
          ))}
        </div>
      </PageHeader>
      {error && <ErrorNote error={error} />}
      {loading && <Skeleton />}
      {data && tasks.length === 0 && (
        <Empty title={filter === 'done' ? 'Nothing finished yet' : 'No tasks'}>
          Create one with <code>agentvc task new "goal"</code>.
        </Empty>
      )}
      {tasks.length > 0 && <TaskList key={filter} tasks={tasks} group="Tasks" />}
    </div>
  )
}
