// Resolve conflicts in place: each conflicting hunk shows both sides with
// Keep main / Keep mine / Keep both / Edit. Used for a version (an attempt
// after a rebase) and for a stacked branch whose restack stopped.

import { useState } from 'react'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { ConflictHunk, GraphAttempt, HunkTake } from '../api/types'
import { useResource } from '../api/useResource'
import { versionName, WORDS } from '../core/words'
import { plural } from '../lib/format'
import { ErrorNote, Skeleton } from './bits'
import { useToast } from './Toaster'

/** "main" for a rebase onto main; otherwise the other side's own label. */
function theirsName(h: ConflictHunk): string {
  const l = h.theirs_label.trim()
  if (!l || /^[0-9a-f]{7,40}$/.test(l) || /^(main|master|origin\/)/.test(l)) return 'main'
  return l
}

export function ConflictResolver({ version, rechoose }: { version: GraphAttempt; rechoose?: boolean }) {
  const a = version.attempt
  const data = useResource(`conflicts:${a.id}:${version.conflicts}`, () => api.conflicts(a.id))
  const toast = useToast()
  const { refresh } = useLive()
  const name = versionName(a)
  const files = data.data?.files ?? []

  const choose = async (path: string, h: ConflictHunk, take: HunkTake, custom?: string) => {
    try {
      const r = await api.resolve(a.id, path, [{ index: h.index, take, text: custom }])
      const left = r.resolve.open.length
      if (!left && rechoose) await api.promote(a.id)
      toast.ok(left ? `${path}: resolved. ${plural(left, 'file')} left.` : `All conflicts resolved. ${name} is ready.`, { undo: false })
    } catch (e) {
      toast.error(e)
    } finally {
      refresh()
    }
  }

  return (
    <section className="conf-box" aria-label="Conflicts">
      <header>
        <b>{plural(version.conflicts, 'file')} changed on both sides</b>
        <span className="muted">Pick what each place should say.</span>
      </header>
      {data.error && <ErrorNote error={data.error} />}
      {!data.data && !data.error && <Skeleton rows={3} />}
      {files.map((f) => (
        <HunkList key={f.path} path={f.path} hunks={f.hunks} mine={name} onChoose={(h, take, text) => choose(f.path, h, take, text)} />
      ))}
    </section>
  )
}

/** The hunks of one file, each with Keep {theirs} / Keep {mine} / Keep both / Edit. */
export function HunkList({
  path,
  hunks,
  mine,
  onChoose,
}: {
  path: string
  hunks: ConflictHunk[]
  mine: string
  onChoose: (h: ConflictHunk, take: HunkTake, custom?: string) => Promise<void>
}) {
  const [busy, setBusy] = useState(false)
  const [editing, setEditing] = useState<number | null>(null)
  const [text, setText] = useState('')

  const choose = async (h: ConflictHunk, take: HunkTake, custom?: string) => {
    setBusy(true)
    try {
      await onChoose(h, take, custom)
      setEditing(null)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="conf-file">
      <div className="conf-path mono">{path}</div>
      {hunks.length === 0 && <p className="muted">No conflict markers in this file (deleted on one side, or binary). Resolve it in your editor.</p>}
      {hunks.map((h) => {
        const other = theirsName(h)
        return (
          <div key={h.index} className="conf-hunk">
            <div className="conf-sides">
              <div>
                <div className="conf-label">{other}</div>
                <pre className="conf-code">{h.theirs || ' '}</pre>
              </div>
              <div>
                <div className="conf-label">{mine}</div>
                <pre className="conf-code">{h.ours || ' '}</pre>
              </div>
            </div>
            {editing === h.index ? (
              <div className="conf-edit">
                <textarea
                  id={`conf-${path}-${h.index}`}
                  aria-label={`Text for ${path}`}
                  value={text}
                  onChange={(e) => setText(e.target.value)}
                  rows={Math.max(3, text.split('\n').length + 1)}
                />
                <div className="conf-opts">
                  <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void choose(h, 'custom', text.endsWith('\n') || !text ? text : text + '\n')}>
                    Use this text
                  </button>
                  <button type="button" className="btn btn-ghost" onClick={() => setEditing(null)}>
                    Cancel
                  </button>
                </div>
              </div>
            ) : (
              <div className="conf-opts">
                <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => void choose(h, 'theirs')}>
                  {other === 'main' ? WORDS.keepMain : WORDS.keep(other)}
                </button>
                <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => void choose(h, 'ours')}>
                  {WORDS.keep(mine)}
                </button>
                <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void choose(h, 'both')}>
                  {WORDS.keepBoth}
                </button>
                <button
                  type="button"
                  className="btn btn-ghost"
                  disabled={busy}
                  onClick={() => {
                    setText(h.theirs + h.ours)
                    setEditing(h.index)
                  }}
                >
                  Edit…
                </button>
              </div>
            )}
          </div>
        )
      })}
    </div>
  )
}
