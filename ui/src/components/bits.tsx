import type { ReactNode } from 'react'
import type { ApiError } from '../api/client'
import { STATUSES, type Status, type StatusCounts } from '../api/types'
import { keyLabel } from '../keyboard/hotkeys'

export function Kbd({ keys }: { keys: string }) {
  return (
    <span className="kbd-group">
      {keyLabel(keys).map((k, i) => (
        <kbd key={i} className="kbd">
          {k}
        </kbd>
      ))}
    </span>
  )
}

const STATUS_LABEL: Record<Status, string> = {
  exploring: 'Exploring',
  passed: 'Passed',
  failed: 'Failed',
  abandoned: 'Abandoned',
  promoted: 'Promoted',
}

export function StatusPill({ status }: { status: Status }) {
  return (
    <span className={`pill pill-${status}`}>
      <span className="dot" />
      {STATUS_LABEL[status]}
    </span>
  )
}

/** Proportional bar of attempt outcomes, with a tooltip of counts. */
export function OutcomeBar({ counts }: { counts: StatusCounts }) {
  const title = STATUSES.filter((s) => counts[s] > 0)
    .map((s) => `${counts[s]} ${s}`)
    .join(', ')
  if (counts.total === 0) return <span className="outcome-bar empty" title="No attempts yet" />
  return (
    <span className="outcome-bar" title={title}>
      {STATUSES.map((s) =>
        counts[s] > 0 ? (
          <span key={s} className={`seg seg-${s}`} style={{ flexGrow: counts[s] }} />
        ) : null,
      )}
    </span>
  )
}

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      <div className="empty-title">{title}</div>
      {children && <div className="empty-body">{children}</div>}
    </div>
  )
}

export function ErrorNote({ error }: { error: ApiError }) {
  return (
    <div className="error-note" role="alert">
      <code>{error.code}</code> {error.message}
    </div>
  )
}

export function Skeleton({ rows = 4 }: { rows?: number }) {
  return (
    <div className="skeleton" aria-busy="true">
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="skeleton-row" style={{ opacity: 1 - i * 0.18 }} />
      ))}
    </div>
  )
}

export function PageHeader({
  title,
  eyebrow,
  children,
}: {
  title: ReactNode
  eyebrow?: ReactNode
  children?: ReactNode
}) {
  return (
    <header className="page-header">
      <div>
        {eyebrow && <div className="eyebrow">{eyebrow}</div>}
        <h1>{title}</h1>
      </div>
      {children && <div className="page-actions">{children}</div>}
    </header>
  )
}
