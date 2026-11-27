import { useMemo, useState } from 'react'
import { useNavigate } from 'react-router'
import { api } from '../api/client'
import { useResource } from '../api/useResource'
import { useHotkeys } from '../keyboard/hotkeys'
import { actorName } from '../lib/format'
import { StatusPill } from './bits'
import { Icon, type IconName } from './Icon'
import type { Status } from '../api/types'

interface Item {
  id: string
  icon: IconName
  label: string
  hint?: string
  status?: Status
  to: string
}

function score(item: Item, q: string): number {
  if (!q) return 1
  const hay = `${item.id} ${item.label} ${item.hint ?? ''}`.toLowerCase()
  if (item.id.toLowerCase() === q) return 100
  const words = q.split(/\s+/).filter(Boolean)
  if (!words.every((w) => hay.includes(w))) return 0
  return hay.startsWith(q) ? 10 : item.label.toLowerCase().includes(q) ? 5 : 1
}

export function CommandPalette({ onClose }: { onClose: () => void }) {
  const navigate = useNavigate()
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState(0)
  const tasks = useResource('tasks', api.tasks)
  const attempts = useResource('attempts', () => api.attempts())

  const items = useMemo<Item[]>(() => {
    const out: Item[] = [
      { id: 'review', icon: 'inbox', label: 'Home', hint: 'Your stack and agent work', to: '/review' },
      { id: 'find', icon: 'search', label: 'Find', hint: 'Search the code and see who wrote each line', to: '/find' },
      { id: 'agent-permissions', icon: 'check', label: 'Agent permissions', hint: 'What agents may do, per workspace', to: '/settings/agents' },
    ]
    for (const t of tasks.data ?? []) {
      out.push({ id: `t${t.id}`, icon: 'tasks', label: t.goal, hint: `${t.attempts.total} tries`, to: `/review/t/${t.id}` })
    }
    for (const a of attempts.data ?? []) {
      out.push({
        id: `a${a.id}`,
        icon: 'branch',
        label: a.id === 1 ? 'Working directory' : `${actorName(a.actor)}${a.model ? ` · ${a.model}` : ''}`,
        hint: a.task_id ? `t${a.task_id}` : undefined,
        status: a.status,
        to: `/attempts/${a.id}`,
      })
    }
    return out
  }, [tasks.data, attempts.data])

  const q = query.trim().toLowerCase()
  const results = items
    .map((item) => ({ item, s: score(item, q) }))
    .filter((r) => r.s > 0)
    .sort((a, b) => b.s - a.s)
    .slice(0, 50)
    .map((r) => r.item)
  const current = Math.min(selected, results.length - 1)

  const go = (item: Item | undefined) => {
    if (!item) return
    onClose()
    navigate(item.to)
  }

  useHotkeys(
    [
      { keys: 'esc', description: 'Close', handler: onClose, inInputs: true },
      {
        keys: ['down', 'mod+j'],
        description: 'Next',
        inInputs: true,
        handler: () => setSelected(Math.min(current + 1, results.length - 1)),
      },
      {
        keys: ['up', 'mod+k'],
        description: 'Previous',
        inInputs: true,
        handler: () => setSelected(Math.max(current - 1, 0)),
      },
      { keys: 'enter', description: 'Open', inInputs: true, handler: () => go(results[current]) },
    ],
    { exclusive: true },
  )

  return (
    <div className="overlay overlay-top" onMouseDown={onClose}>
      <div
        className="dialog palette"
        role="dialog"
        aria-label="Search and jump"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="palette-input">
          <Icon name="search" />
          <input
            autoFocus
            value={query}
            placeholder="Jump to a task, attempt (a3) or page…"
            onChange={(e) => {
              setQuery(e.target.value)
              setSelected(0)
            }}
          />
        </div>
        <ul className="palette-list" role="listbox">
          {results.length === 0 && <li className="palette-empty">No matches</li>}
          {results.map((item, i) => (
            <li
              key={item.id}
              role="option"
              aria-selected={i === current}
              className="palette-item"
              onMouseMove={() => setSelected(i)}
              onClick={() => go(item)}
            >
              <Icon name={item.icon} />
              <span className="palette-id">{/^[ta]\d+$/.test(item.id) ? item.id : ''}</span>
              <span className="palette-label">{item.label}</span>
              {item.status && <StatusPill status={item.status} />}
              {item.hint && <span className="palette-hint">{item.hint}</span>}
            </li>
          ))}
        </ul>
      </div>
    </div>
  )
}
