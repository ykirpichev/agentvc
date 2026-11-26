import { memo, useMemo } from 'react'
import { Link } from 'react-router'
import { api } from '../api/client'
import type { BlameLine, BlameOrigin } from '../api/types'
import { useResource } from '../api/useResource'
import { actorName, fullDate, timeAgo } from '../lib/format'
import { ErrorNote, Skeleton } from './bits'
import { Icon } from './Icon'

/** Rows per chunk. Each chunk is skipped by layout and paint while off screen. */
const CHUNK = 200

interface Run {
  origin: BlameOrigin
  start: number
  lines: BlameLine[]
}

function originKey(o: BlameOrigin): string {
  switch (o.kind) {
    case 'snapshot':
      return `s${o.snapshot}`
    case 'git':
      return `git:${o.commit}`
    case 'working':
      return 'working'
  }
}

/** Stable color slot per author of a line: an attempt, git, or the unsaved view. */
function colorSlot(o: BlameOrigin, current: number): string {
  if (o.kind === 'git') return 'git'
  if (o.kind === 'working') return 'working'
  if (o.attempt === current) return 'self'
  return `c${o.attempt % 6}`
}

function Label({ origin: o, current }: { origin: BlameOrigin; current: number }) {
  if (o.kind === 'git') {
    return (
      <span className="blame-label" title={`${o.commit}\n${o.author} · ${fullDate(o.time)}\n${o.summary}`}>
        <code>{o.commit.slice(0, 7)}</code> <span className="blame-who">{o.author}</span>
        <span className="blame-when">{timeAgo(o.time)}</span>
      </span>
    )
  }
  if (o.kind === 'working') {
    return (
      <span className="blame-label" title="In the view, not yet snapshotted">
        <code>a{o.attempt}</code> <span className="blame-who">not snapshotted</span>
      </span>
    )
  }
  const who = actorName(o.actor)
  return (
    <Link
      to={`/attempts/${o.attempt}?snap=${o.snapshot}`}
      className="blame-label"
      title={`s${o.snapshot} · ${o.trigger}${o.source ? ` from ${o.source}` : ''}\n${o.actor}${o.model ? ` · ${o.model}` : ''}\n${fullDate(o.created_at)}`}
    >
      <code>
        {o.attempt === current ? 'this' : `a${o.attempt}`} s{o.snapshot}
      </code>{' '}
      <span className="blame-who">
        {who}
        {o.source && <span className="blame-src"> ← {o.source}</span>}
      </span>
      <span className="blame-when">{timeAgo(o.created_at)}</span>
    </Link>
  )
}

const Chunk = memo(function Chunk({ runs, current }: { runs: Run[]; current: number }) {
  const count = runs.reduce((n, r) => n + r.lines.length, 0)
  return (
    <div className="blame-chunk" style={{ containIntrinsicSize: `auto ${count * 20}px` }}>
      {runs.map((r) => (
        <div key={r.start} className={`blame-run blame-${colorSlot(r.origin, current)}`}>
          <div className="blame-gutter">
            <Label origin={r.origin} current={current} />
          </div>
          <div className="blame-lines">
            {r.lines.map((l) => (
              <div key={l.line} className="blame-line">
                <span className="ln">{l.line}</span>
                <span className="code">{l.text || ' '}</span>
              </div>
            ))}
          </div>
        </div>
      ))}
    </div>
  )
})

/** Every line of `path` in attempt `attempt`, with the snapshot or commit that wrote it. */
export function BlameView({ attempt, path }: { attempt: number; path: string }) {
  const blame = useResource(`blame:a${attempt}:${path}`, () => api.blame(attempt, path))
  const lines = blame.data?.lines
  // Runs of consecutive lines with one origin, packed into chunks of about
  // CHUNK rows (a run is split at a chunk edge and labelled again).
  const { chunks, authors } = useMemo(() => {
    const chunks: Run[][] = []
    const authors = new Set<string>()
    let cur: Run[] = []
    let rows = 0
    let prev = ''
    for (const l of lines ?? []) {
      const k = originKey(l.origin)
      authors.add(l.origin.kind === 'snapshot' ? `a${l.origin.attempt}` : l.origin.kind)
      if (rows >= CHUNK) {
        chunks.push(cur)
        cur = []
        rows = 0
        prev = ''
      }
      const last = cur[cur.length - 1]
      if (last && k === prev) last.lines.push(l)
      else cur.push({ origin: l.origin, start: l.line, lines: [l] })
      prev = k
      rows++
    }
    if (cur.length) chunks.push(cur)
    return { chunks, authors }
  }, [lines])

  const own = lines?.filter((l) => l.origin.kind === 'snapshot' && l.origin.attempt === attempt).length ?? 0
  const unsaved = lines?.filter((l) => l.origin.kind === 'working').length ?? 0

  return (
    <section className="diff blame">
      <header className="diff-head">
        <Icon name="file" />
        <span className="diff-path">{path}</span>
        <span className="change">blame</span>
        {lines && (
          <span className="diff-stat blame-stat">
            {lines.length} lines · {own} from this attempt
            {unsaved > 0 && ` · ${unsaved} unsaved`} · {authors.size} {authors.size === 1 ? 'source' : 'sources'}
          </span>
        )}
      </header>
      {blame.error && <ErrorNote error={blame.error} />}
      {blame.loading && <Skeleton rows={8} />}
      {lines && lines.length === 0 && <div className="diff-note">Empty file</div>}
      {lines && lines.length > 0 && (
        <div className="blame-body">
          {chunks.map((c, i) => (
            <Chunk key={i} runs={c} current={attempt} />
          ))}
        </div>
      )}
    </section>
  )
}
