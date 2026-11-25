import { useNavigate } from 'react-router'
import type { TaskSummary } from '../api/types'
import { useListNavigation } from '../keyboard/useListNavigation'
import { timeAgo } from '../lib/format'
import { OutcomeBar } from './bits'

function Counts({ t }: { t: TaskSummary }) {
  const a = t.attempts
  return (
    <span className="counts">
      {a.passed > 0 && <span className="count count-passed">{a.passed} passed</span>}
      {a.failed > 0 && <span className="count count-failed">{a.failed} failed</span>}
      {a.exploring > 0 && <span className="count count-exploring">{a.exploring} running</span>}
      {a.promoted > 0 && <span className="count count-promoted">picked</span>}
      {a.total === 0 && <span className="count">no attempts</span>}
    </span>
  )
}

/** Keyboard-navigable list of tasks. */
export function TaskList({ tasks, group }: { tasks: TaskSummary[]; group: string }) {
  const navigate = useNavigate()
  const open = (i: number) => {
    const t = tasks[i]
    if (t) navigate(`/tasks/${t.id}`)
  }
  const { selected, setSelected, rowRef } = useListNavigation(tasks.length, {
    onOpen: open,
    group,
    noun: 'task',
  })
  return (
    <ul className="rows" role="listbox">
      {tasks.map((t, i) => (
        <li
          key={t.id}
          ref={rowRef(i)}
          role="option"
          aria-selected={i === selected}
          className="row task-row"
          onMouseEnter={() => setSelected(i)}
          onClick={() => open(i)}
        >
          <span className="row-id">t{t.id}</span>
          <span className="row-main">
            <span className="row-title">{t.goal}</span>
            {t.criteria && <span className="row-sub">{t.criteria}</span>}
          </span>
          <Counts t={t} />
          <OutcomeBar counts={t.attempts} />
          <span className="row-time" title={new Date(t.updated_at * 1000).toLocaleString()}>
            {timeAgo(t.updated_at)}
          </span>
        </li>
      ))}
    </ul>
  )
}
