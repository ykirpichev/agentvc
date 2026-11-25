import { Link } from 'react-router'
import type { Attempt } from '../api/types'
import { actorName, fullDate, timeAgo } from '../lib/format'

/** Provenance block: who, which model, from where, and the prompt. */
export function AttemptMeta({ attempt, extra }: { attempt: Attempt; extra?: React.ReactNode }) {
  return (
    <div className="meta">
      <dl className="meta-grid">
        <div>
          <dt>Actor</dt>
          <dd>{actorName(attempt.actor)}</dd>
        </div>
        <div>
          <dt>Model</dt>
          <dd>{attempt.model ?? <span className="muted">—</span>}</dd>
        </div>
        <div>
          <dt>Forked from</dt>
          <dd>
            {attempt.parent_attempt ? (
              <Link to={`/attempts/${attempt.parent_attempt}`} className="ref">
                a{attempt.parent_attempt}
              </Link>
            ) : (
              <span className="muted">—</span>
            )}
            {attempt.parent_snapshot && <span className="muted"> @ s{attempt.parent_snapshot}</span>}
          </dd>
        </div>
        <div>
          <dt>Started</dt>
          <dd title={fullDate(attempt.created_at)}>{timeAgo(attempt.created_at)}</dd>
        </div>
        {extra}
      </dl>
      {attempt.reason && (
        <div className="reason">
          <span className="label">Reason</span> {attempt.reason}
        </div>
      )}
      {attempt.prompt && (
        <blockquote className="prompt">
          <span className="label">Prompt</span>
          {attempt.prompt}
        </blockquote>
      )}
    </div>
  )
}
