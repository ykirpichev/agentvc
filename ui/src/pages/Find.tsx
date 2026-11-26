import { useEffect, useRef, useState } from 'react'
import { Link, useNavigate, useSearchParams } from 'react-router'
import { api } from '../api/client'
import { STATUSES, type QueryRow, type Status } from '../api/types'
import { useResource } from '../api/useResource'
import { Empty, ErrorNote, PageHeader, Skeleton, StatusPill } from '../components/bits'
import { Icon } from '../components/Icon'
import { useHotkeys } from '../keyboard/hotkeys'
import { useListNavigation } from '../keyboard/useListNavigation'
import { queryFromParams } from '../lib/find'
import { actorName, plural, timeAgo } from '../lib/format'

/** URL keys, in the order the filter bar shows them. Lists are comma-separated. */
const TEXT_FILTERS = [
  { key: 'touched', label: 'Touched', placeholder: 'src/app.py, docs/' },
  { key: 'actor', label: 'Actor', placeholder: 'claude' },
  { key: 'model', label: 'Model', placeholder: 'opus' },
  { key: 'meta', label: 'Meta', placeholder: 'exit_code=0' },
  { key: 'since', label: 'Since', placeholder: '2d' },
  { key: 'grep', label: 'Text', placeholder: 'prompt or reason' },
] as const

const SINCE_PRESETS = ['1h', '1d', '1w']

export function Find() {
  const [params, setParams] = useSearchParams()
  const navigate = useNavigate()
  const q = queryFromParams(params)
  const key = `query:${params.toString()}`
  const rows = useResource(key, () => api.query(q))
  const tasks = useResource('tasks', api.tasks)
  // Newest first: the query returns oldest first, like `agentvc log`.
  const results = [...(rows.data ?? [])].reverse()

  const set = (k: string, v: string | null) => {
    const next = new URLSearchParams(params)
    if (v) next.set(k, v)
    else next.delete(k)
    setParams(next, { replace: true })
  }
  const toggleStatus = (s: Status) => {
    const cur = q.status ?? []
    const next = cur.includes(s) ? cur.filter((x) => x !== s) : [...cur, s]
    set('status', STATUSES.filter((x) => next.includes(x)).join(','))
  }
  const active = [...params.keys()].length
  const open = (i: number) => {
    const r = results[i]
    if (!r) return
    const file = r.touched?.[0]
    navigate(`/attempts/${r.id}${file ? `?file=${encodeURIComponent(file)}` : ''}`)
  }
  const nav = useListNavigation(results.length, { onOpen: open, group: 'Find', noun: 'attempt' })
  const firstInput = useRef<HTMLInputElement>(null)
  useHotkeys([
    {
      keys: 'f',
      description: 'Focus the first filter',
      group: 'Find',
      handler: (e) => {
        e.preventDefault()
        firstInput.current?.focus()
      },
    },
    {
      keys: 'x',
      description: 'Clear all filters',
      group: 'Find',
      handler: () => setParams(new URLSearchParams(), { replace: true }),
    },
    ...STATUSES.map((s, i) => ({
      keys: String(i + 1),
      description: `Toggle ${s}`,
      group: 'Find',
      handler: () => toggleStatus(s),
    })),
  ])

  return (
    <div className="page page-wide">
      <PageHeader
        title="Find attempts"
        eyebrow={rows.data ? `${results.length} ${results.length === 1 ? 'match' : 'matches'}${active ? '' : ' · no filters'}` : 'Query history'}
      >
        {active > 0 && (
          <button className="btn btn-ghost btn-sm" onClick={() => setParams(new URLSearchParams(), { replace: true })}>
            <Icon name="x" size={13} /> Clear filters
          </button>
        )}
      </PageHeader>

      <section className="filter-bar" aria-label="Filters">
        <div className="filter-row">
          <label className="filter">
            <span className="filter-label">Task</span>
            <select value={q.task ? String(q.task) : ''} onChange={(e) => set('task', e.target.value ? `t${e.target.value}` : null)}>
              <option value="">Any task</option>
              {(tasks.data ?? [])
                .slice()
                .sort((a, b) => b.updated_at - a.updated_at)
                .map((t) => (
                  <option key={t.id} value={t.id}>
                    t{t.id} · {t.goal.length > 60 ? `${t.goal.slice(0, 59)}…` : t.goal}
                  </option>
                ))}
            </select>
          </label>
          <div className="filter">
            <span className="filter-label">Status</span>
            <div className="chips" role="group" aria-label="Status">
              {STATUSES.map((s) => (
                <button
                  key={s}
                  className={`chip chip-${s}`}
                  aria-pressed={q.status?.includes(s) ?? false}
                  onClick={() => toggleStatus(s)}
                >
                  {s}
                </button>
              ))}
            </div>
          </div>
        </div>
        <div className="filter-row filter-row-text">
          {TEXT_FILTERS.map((f, i) => (
            <TextFilter
              key={f.key}
              inputRef={i === 0 ? firstInput : undefined}
              label={f.label}
              placeholder={f.placeholder}
              value={params.get(f.key) ?? ''}
              onCommit={(v) => set(f.key, v.trim() || null)}
              presets={f.key === 'since' ? SINCE_PRESETS : undefined}
            />
          ))}
        </div>
      </section>

      {rows.error && <ErrorNote error={rows.error} />}
      {rows.loading && !rows.data && <Skeleton />}
      {rows.data && results.length === 0 && (
        <Empty title="No attempts match">Loosen a filter, or clear them all with x.</Empty>
      )}
      {results.length > 0 && (
        <ul className="rows" role="listbox" aria-label="Matching attempts">
          {results.map((r, i) => (
            <ResultRow
              key={r.id}
              row={r}
              showTask={!q.task}
              selected={i === nav.selected}
              rowRef={nav.rowRef(i)}
              onHover={() => nav.setSelected(i)}
              onOpen={() => open(i)}
            />
          ))}
        </ul>
      )}
    </div>
  )
}

function ResultRow({
  row: r,
  showTask,
  selected,
  rowRef,
  onHover,
  onOpen,
}: {
  row: QueryRow
  showTask: boolean
  selected: boolean
  rowRef: (el: HTMLElement | null) => void
  onHover: () => void
  onOpen: () => void
}) {
  const meta = Object.entries(r.meta).filter(([k]) => k !== 'command' && k !== 'hook')
  return (
    <li
      ref={rowRef}
      role="option"
      aria-selected={selected}
      className="row find-row"
      onMouseEnter={onHover}
      onClick={onOpen}
    >
      <span className="row-id">a{r.id}</span>
      <span className="row-main">
        <span className="row-title">
          {actorName(r.actor)}
          {r.model && <span className="muted"> · {r.model}</span>}
          {showTask && r.task_id && (
            <Link to={`/review/t/${r.task_id}`} className="find-task" onClick={(e) => e.stopPropagation()}>
              t{r.task_id} {r.task}
            </Link>
          )}
        </span>
        {(r.touched?.length || meta.length > 0 || r.reason) && (
          <span className="find-tags">
            {r.touched?.slice(0, 4).map((p) => (
              <span key={p} className="tag tag-file" title={p}>
                <Icon name="file" size={11} /> {p}
              </span>
            ))}
            {(r.touched?.length ?? 0) > 4 && <span className="tag">+{r.touched!.length - 4} more</span>}
            {meta.slice(0, 4).map(([k, v]) => (
              <span key={k} className="tag" title={`${k}=${v}`}>
                {k}={v.length > 24 ? `${v.slice(0, 23)}…` : v}
              </span>
            ))}
            {r.reason && <span className="row-sub find-reason">{r.reason}</span>}
          </span>
        )}
      </span>
      <span className="count">{plural(r.snapshots, 'snapshot')}</span>
      <StatusPill status={r.status} />
      <span className="row-time" title={new Date((r.last_snapshot_at ?? r.created_at) * 1000).toLocaleString()}>
        {timeAgo(r.last_snapshot_at ?? r.created_at)}
      </span>
    </li>
  )
}

/** A text filter that writes to the URL after typing pauses, on enter, or on blur. */
function TextFilter({
  label,
  placeholder,
  value,
  onCommit,
  presets,
  inputRef,
}: {
  label: string
  placeholder: string
  value: string
  onCommit: (v: string) => void
  presets?: string[]
  inputRef?: React.Ref<HTMLInputElement>
}) {
  const [draft, setDraft] = useState(value)
  const [synced, setSynced] = useState(value)
  // Follow the URL when it changes elsewhere (back button, Clear filters).
  if (value !== synced) {
    setSynced(value)
    setDraft(value)
  }
  const commit = useRef(onCommit)
  useEffect(() => {
    commit.current = onCommit
  })
  useEffect(() => {
    if (draft === value) return
    const t = setTimeout(() => commit.current(draft), 400)
    return () => clearTimeout(t)
  }, [draft, value])

  return (
    <label className="filter filter-text">
      <span className="filter-label">{label}</span>
      <span className="filter-input">
        <input
          ref={inputRef}
          value={draft}
          placeholder={placeholder}
          spellCheck={false}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={() => draft !== value && onCommit(draft)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') onCommit(draft)
            if (e.key === 'Escape') (e.target as HTMLInputElement).blur()
          }}
        />
        {draft && (
          <button
            className="filter-clear"
            aria-label={`Clear ${label.toLowerCase()}`}
            onClick={(e) => {
              e.preventDefault()
              setDraft('')
              onCommit('')
            }}
          >
            <Icon name="x" size={11} />
          </button>
        )}
      </span>
      {presets && !draft && (
        <span className="filter-presets">
          {presets.map((p) => (
            <button
              key={p}
              className="chip chip-sm"
              onClick={(e) => {
                e.preventDefault()
                onCommit(p)
              }}
            >
              {p}
            </button>
          ))}
        </span>
      )}
    </label>
  )
}
