import { structuredPatch } from 'diff'
import { Fragment, useState } from 'react'
import { Link } from 'react-router'
import { codeLink } from '../core/links'
import { inEditor, openDiffInEditor, openFileInEditor } from '../core/editor'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { ChangeKind, FileResponse, Ref } from '../api/types'
import { useResource } from '../api/useResource'
import { bytes } from '../lib/format'
import { ErrorNote, Skeleton } from './bits'
import { CommentBox, ThreadView } from './Comments'
import { lineKey, placeThreads, type DiffComments } from '../core/comments'
import { Icon } from './Icon'
import { useToast } from './Toaster'
import type { CommentSide, CommentThread } from '../api/types'

interface Line {
  kind: 'add' | 'del' | 'ctx'
  text: string
  old?: number
  new?: number
}

interface Hunk {
  /** Unchanged lines skipped before this hunk. */
  skipped: number
  lines: Line[]
}

function buildHunks(oldText: string, newText: string): { hunks: Hunk[]; added: number; removed: number } {
  const patch = structuredPatch('a', 'b', oldText, newText, '', '', { context: 3 })
  let added = 0
  let removed = 0
  // Next line number on each side after the previous hunk. Unchanged lines
  // between hunks are the same on both sides, so either side measures the gap.
  let nextOld = 1
  let nextNew = 1
  const hunks = patch.hunks.map((h) => {
    let o = h.oldStart
    let n = h.newStart
    const lines: Line[] = []
    for (const raw of h.lines) {
      const tag = raw[0]
      const text = raw.slice(1)
      if (tag === '\\') continue // "\ No newline at end of file"
      if (tag === '+') {
        lines.push({ kind: 'add', text, new: n++ })
        added++
      } else if (tag === '-') {
        lines.push({ kind: 'del', text, old: o++ })
        removed++
      } else {
        lines.push({ kind: 'ctx', text, old: o++, new: n++ })
      }
    }
    const firstOld = lines.find((l) => l.old !== undefined)?.old
    const firstNew = lines.find((l) => l.new !== undefined)?.new
    const skipped = Math.max(
      0,
      firstOld !== undefined ? firstOld - nextOld : firstNew !== undefined ? firstNew - nextNew : 0,
    )
    nextOld = o
    nextNew = n
    return { skipped, lines }
  })
  return { hunks, added, removed }
}

function content(f: FileResponse | undefined): string | null {
  if (!f) return ''
  return f.content
}

/** Lines picked for a new comment: one side, a range. */
interface Pick {
  side: CommentSide
  start: number
  end: number
}

/** The side and line a row is commented on: removed lines on the old side, the rest on the new. */
function rowLine(l: Line): { side: CommentSide; line: number } {
  return l.kind === 'del' ? { side: 'old', line: l.old! } : { side: 'new', line: l.new! }
}

/** Unified diff of one file between two refs. With `comments`, lines take
 * review comments: click a line number (or drag over several) to write one;
 * threads show under their line. */
export function DiffView({
  base,
  head,
  path,
  kind,
  comments,
  codeRef,
  localFile,
}: {
  base: Ref
  head: Ref
  path: string
  kind: ChangeKind
  comments?: DiffComments
  /** A browse ref (`branch:w1/x`, `agent:ag3`) to offer "View file" in the Code tab. */
  codeRef?: string
  /** The file on this machine (the checkout or agent worktree holding `head`), opened in the editor. */
  localFile?: string
}) {
  const key = `${base}..${head}:${path}`
  // Diff inside the loader so it runs once per fetch, not per render.
  const files = useResource(key, async () => {
    const [a, b] = await Promise.all([
      kind === 'added' ? undefined : api.file(base, path),
      kind === 'removed' ? undefined : api.file(head, path),
    ])
    const ta = content(a)
    const tb = content(b)
    if (ta === null || tb === null) return { binary: true as const, a, b }
    return { binary: false as const, ...buildHunks(ta, tb) }
  })
  const result = files.data
  const toast = useToast()
  const { refresh } = useLive()
  const [pick, setPick] = useState<Pick | null>(null)
  const [dragging, setDragging] = useState(false)
  const placed = comments ? placeThreads(comments.threads, path) : null
  // Threads whose line is not among the rows shown (or gone) go above the table.
  const shown = new Set<string>()
  if (result && !result.binary)
    for (const h of result.hunks)
      for (const l of h.lines) {
        const r = rowLine(l)
        shown.add(lineKey(r.side, r.line))
        if (l.kind === 'ctx') shown.add(lineKey('old', l.old!))
      }
  const above: CommentThread[] = placed
    ? [...placed.loose, ...[...placed.at.entries()].filter(([k]) => !shown.has(k)).flatMap(([, ts]) => ts)]
    : []
  const canWrite = !!comments && !comments.readOnly

  const startPick = (l: Line) => {
    if (!canWrite) return
    const r = rowLine(l)
    setPick({ side: r.side, start: r.line, end: r.line })
    setDragging(true)
  }
  const extendPick = (l: Line) => {
    if (!dragging || !pick) return
    const r = rowLine(l)
    if (r.side !== pick.side) return
    setPick({ ...pick, end: r.line })
  }
  const save = async (body: string, now: boolean) => {
    if (!pick || !comments) return
    const [start, end] = pick.start <= pick.end ? [pick.start, pick.end] : [pick.end, pick.start]
    try {
      await api.addComment({ target: comments.target, lines: { path, side: pick.side, start, end }, body, pending: !now })
      setPick(null)
      toast.ok(now ? 'Comment posted.' : 'Added to your review. Send it from the bar above when you are done.', { undo: false })
    } catch (e) {
      toast.error(e)
      throw e
    } finally {
      refresh()
    }
  }
  const inPick = (l: Line) => {
    if (!pick) return false
    const r = rowLine(l)
    const [a, b] = pick.start <= pick.end ? [pick.start, pick.end] : [pick.end, pick.start]
    return r.side === pick.side && r.line >= a && r.line <= b
  }
  const pickEnd = pick ? Math.max(pick.start, pick.end) : 0
  const rowExtras = (l: Line) => {
    const r = rowLine(l)
    const ts = [...(placed?.at.get(lineKey(r.side, r.line)) ?? []), ...(l.kind === 'ctx' ? (placed?.at.get(lineKey('old', l.old!)) ?? []) : [])]
    const box = pick && !dragging && r.side === pick.side && r.line === pickEnd
    if (ts.length === 0 && !box) return null
    return (
      <tr className="dl-threads">
        <td colSpan={3}>
          {ts.map((t) => (
            <ThreadView key={t.id} t={t} readOnly={comments?.readOnly} />
          ))}
          {box && (
            <CommentBox
              label={`${pick.side === 'old' ? 'Old line' : 'Line'}${pick.start === pick.end ? ` ${pick.start}` : `s ${Math.min(pick.start, pick.end)}-${pickEnd}`}`}
              onSave={save}
              onCancel={() => setPick(null)}
            />
          )}
        </td>
      </tr>
    )
  }

  return (
    <section className="diff">
      <header className="diff-head">
        <Icon name="file" />
        <span className="diff-path">{path}</span>
        <span className={`change change-${kind}`}>{kind}</span>
        {result && !result.binary && (
          <span className="diff-stat">
            <span className="add">+{result.added}</span>
            <span className="del">−{result.removed}</span>
          </span>
        )}
        {inEditor ? (
          <>
            <button type="button" className="linkish diff-view-file" onClick={() => openDiffInEditor(base, head, path)} title="Show this change in the editor's diff view">
              Open diff
            </button>
            {kind !== 'removed' && (
              <button
                type="button"
                className="linkish diff-view-file"
                onClick={() => openFileInEditor(head, path, localFile)}
                title={localFile ? `Open ${localFile} in the editor` : 'Open this version in the editor, read-only'}
              >
                Open file
              </button>
            )}
          </>
        ) : (
          codeRef &&
          kind !== 'removed' && (
            <Link className="diff-view-file" to={codeLink(codeRef, path)} title="See the whole file in the Code tab">
              View file
            </Link>
          )
        )}
      </header>
      {files.error && <ErrorNote error={files.error} />}
      {files.loading && <Skeleton rows={6} />}
      {result?.binary && (
        <div className="diff-note">
          Binary or large file
          {result.b && ` · ${bytes(result.b.size)}`}
          {result.a && result.b && ` (was ${bytes(result.a.size)})`}
        </div>
      )}
      {result && !result.binary && result.hunks.length === 0 && (
        <div className="diff-note">No textual changes (mode or empty file)</div>
      )}
      {above.length > 0 && (
        <div className="diff-threads-above">
          {above.map((t) => (
            <ThreadView key={t.id} t={t} readOnly={comments?.readOnly} showWhere />
          ))}
        </div>
      )}
      {result && !result.binary && result.hunks.length > 0 && (
        <table className={`diff-table ${canWrite ? 'diff-commentable' : ''}`} onMouseUp={() => setDragging(false)} onMouseLeave={() => setDragging(false)}>
          <colgroup>
            <col className="col-ln" />
            <col className="col-ln" />
            <col />
          </colgroup>
          <tbody>
            {result.hunks.map((h, hi) => (
              <HunkRows key={hi} hunk={h} onPick={startPick} onExtend={extendPick} picked={inPick} extras={rowExtras} commentable={canWrite} />
            ))}
          </tbody>
        </table>
      )}
    </section>
  )
}

function HunkRows({
  hunk,
  onPick,
  onExtend,
  picked,
  extras,
  commentable,
}: {
  hunk: Hunk
  onPick: (l: Line) => void
  onExtend: (l: Line) => void
  picked: (l: Line) => boolean
  extras: (l: Line) => React.ReactNode
  commentable: boolean
}) {
  return (
    <>
      {hunk.skipped > 0 && (
        <tr className="diff-gap">
          <td colSpan={3}>
            ⋯ {hunk.skipped} unchanged line{hunk.skipped === 1 ? '' : 's'}
          </td>
        </tr>
      )}
      {hunk.lines.map((l, i) => {
        const gutter = commentable
          ? {
              onMouseDown: (e: React.MouseEvent) => {
                if (e.button !== 0) return
                e.preventDefault()
                onPick(l)
              },
              onMouseEnter: () => onExtend(l),
              title: 'Comment on this line (drag for several)',
            }
          : {}
        return (
          <Fragment key={i}>
            <tr className={`dl dl-${l.kind} ${picked(l) ? 'dl-picked' : ''}`}>
              <td className="ln" {...gutter}>
                {commentable && l.kind === 'del' && <span className="ln-add" aria-hidden>+</span>}
                {l.old ?? ''}
              </td>
              <td className="ln" {...gutter}>
                {commentable && l.kind !== 'del' && <span className="ln-add" aria-hidden>+</span>}
                {l.new ?? ''}
              </td>
              <td className="code">
                <span className="sign">{l.kind === 'add' ? '+' : l.kind === 'del' ? '−' : ' '}</span>
                {l.text || ' '}
              </td>
            </tr>
            {extras(l)}
          </Fragment>
        )
      })}
    </>
  )
}
