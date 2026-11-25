import { structuredPatch } from 'diff'
import { api } from '../api/client'
import type { ChangeKind, FileResponse, Ref } from '../api/types'
import { useResource } from '../api/useResource'
import { bytes } from '../lib/format'
import { ErrorNote, Skeleton } from './bits'
import { Icon } from './Icon'

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

/** Unified diff of one file between two refs. */
export function DiffView({
  base,
  head,
  path,
  kind,
}: {
  base: Ref
  head: Ref
  path: string
  kind: ChangeKind
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
      {result && !result.binary && result.hunks.length > 0 && (
        <table className="diff-table">
          <colgroup>
            <col className="col-ln" />
            <col className="col-ln" />
            <col />
          </colgroup>
          <tbody>
            {result.hunks.map((h, hi) => (
              <HunkRows key={hi} hunk={h} />
            ))}
          </tbody>
        </table>
      )}
    </section>
  )
}

function HunkRows({ hunk }: { hunk: Hunk }) {
  return (
    <>
      {hunk.skipped > 0 && (
        <tr className="diff-gap">
          <td colSpan={3}>
            ⋯ {hunk.skipped} unchanged line{hunk.skipped === 1 ? '' : 's'}
          </td>
        </tr>
      )}
      {hunk.lines.map((l, i) => (
        <tr key={i} className={`dl dl-${l.kind}`}>
          <td className="ln">{l.old ?? ''}</td>
          <td className="ln">{l.new ?? ''}</td>
          <td className="code">
            <span className="sign">{l.kind === 'add' ? '+' : l.kind === 'del' ? '−' : ' '}</span>
            {l.text || ' '}
          </td>
        </tr>
      ))}
    </>
  )
}
