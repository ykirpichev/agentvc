import { useNavigate } from 'react-router'
import { api } from '../api/client'
import type { InboxItem } from '../api/types'
import { useResource } from '../api/useResource'
import { Empty, ErrorNote, Kbd, OutcomeBar, PageHeader, Skeleton } from '../components/bits'
import { Icon } from '../components/Icon'
import { useHotkeys } from '../keyboard/hotkeys'
import { useListNavigation } from '../keyboard/useListNavigation'
import { money, timeAgo } from '../lib/format'

const REASON: Record<string, { label: string; cls: string }> = {
  ready_to_pick: { label: 'Ready to pick', cls: 'reason-ready' },
  failed: { label: 'Failed run', cls: 'reason-failed' },
  checks_failed: { label: 'PR checks fail', cls: 'reason-failed' },
  changes_requested: { label: 'Changes requested', cls: 'reason-failed' },
}

export function Inbox() {
  const navigate = useNavigate()
  const { data, error, loading } = useResource('inbox', api.inbox)
  const items = data ?? []
  const open = (i: number) => {
    const t = items[i]
    if (t) navigate(`/tasks/${t.task.id}`)
  }
  const compare = (i: number) => {
    const t = items[i]
    if (t && t.candidates.length >= 2) navigate(`/compare?a=${t.candidates.slice(0, 4).join(',')}`)
  }
  const { selected, setSelected, rowRef } = useListNavigation(items.length, {
    onOpen: open,
    group: 'Inbox',
    noun: 'task',
  })
  useHotkeys([
    { keys: 'c', description: 'Compare passed attempts', group: 'Inbox', handler: () => compare(selected) },
  ])

  return (
    <div className="page">
      <PageHeader title="Inbox" eyebrow="Waiting on you">
        <span className="muted">
          <Kbd keys="j" /> <Kbd keys="k" /> move · <Kbd keys="enter" /> open · <Kbd keys="c" /> compare
        </span>
      </PageHeader>
      {error && <ErrorNote error={error} />}
      {loading && <Skeleton />}
      {data && items.length === 0 && (
        <Empty title="Inbox zero">
          Tasks show up here when attempts pass or fail and no winner has been picked.
        </Empty>
      )}
      {items.length > 0 && (
        <ul className="rows" role="listbox">
          {items.map((t, i) => (
            <InboxRow
              key={t.task.id}
              item={t}
              selected={i === selected}
              rowRef={rowRef(i)}
              onHover={() => setSelected(i)}
              onOpen={() => open(i)}
              onCompare={() => compare(i)}
            />
          ))}
        </ul>
      )}
    </div>
  )
}

function InboxRow({
  item,
  selected,
  rowRef,
  onHover,
  onOpen,
  onCompare,
}: {
  item: InboxItem
  selected: boolean
  rowRef: (el: HTMLElement | null) => void
  onHover: () => void
  onOpen: () => void
  onCompare: () => void
}) {
  const a = item.attempts
  return (
    <li
      ref={rowRef}
      role="option"
      aria-selected={selected}
      className="row task-row inbox-row"
      onMouseEnter={onHover}
      onClick={onOpen}
    >
      <span className="row-id">t{item.task.id}</span>
      <span className="row-main">
        <span className="row-title">{item.task.goal}</span>
        <span className="row-sub inbox-sub">
          {item.reasons.map((r) => (
            <span key={r} className={`reason-chip ${REASON[r]?.cls ?? ''}`}>
              {REASON[r]?.label ?? r}
            </span>
          ))}
          <span className="counts">
            {a.passed > 0 && <span className="count count-passed">{a.passed} passed</span>}
            {a.failed > 0 && <span className="count count-failed">{a.failed} failed</span>}
            {a.exploring > 0 && <span className="count count-exploring">{a.exploring} running</span>}
            {a.abandoned > 0 && <span className="count">{a.abandoned} abandoned</span>}
          </span>
        </span>
      </span>
      <span
        className={`stat-chip ${item.best_check === 'passed' ? 'chip-ok' : item.best_check === 'failed' ? 'chip-bad' : 'chip-none'}`}
        title="Best check result among open attempts"
      >
        <Icon name="flask" size={13} />
        {item.best_check === 'passed' ? 'Checks pass' : item.best_check === 'failed' ? 'Checks fail' : 'No checks'}
      </span>
      <span className="stat-chip" title="Total reported cost">
        <Icon name="coin" size={13} />
        {item.cost_usd != null ? money(item.cost_usd) : '—'}
      </span>
      <OutcomeBar counts={a} />
      {item.candidates.length >= 2 ? (
        <button
          className="btn btn-ghost btn-sm"
          onClick={(e) => {
            e.stopPropagation()
            onCompare()
          }}
        >
          <Icon name="compare" size={13} /> Compare {Math.min(4, item.candidates.length)}
        </button>
      ) : (
        <span className="btn-sm-spacer" />
      )}
      <span className="row-time" title={new Date(item.updated_at * 1000).toLocaleString()}>
        {timeAgo(item.updated_at)}
      </span>
    </li>
  )
}
