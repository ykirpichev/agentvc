// Resolve conflicts in place: each conflicting hunk shows both sides with
// Keep main / Keep mine / Keep both / Edit. When the last file is resolved
// the daemon records a snapshot and the version is ready again.

import { useState } from 'react'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { ConflictFile, ConflictHunk, GraphAttempt, HunkTake } from '../api/types'
import { useResource } from '../api/useResource'
import { versionName, WORDS } from '../core/words'
import { plural } from '../lib/format'
import { ErrorNote, Skeleton } from './bits'
import { useToast } from './Toaster'

/** "main" for a rebase onto main; otherwise the other side's own label. */
function theirsName(h: ConflictHunk): string {
  const l = h.theirs_label.trim()
  if (!l || /^[0-9a-f]{7,40}$/.test(l) || /main|master|origin\//.test(l)) return 'main'
  return l
}

export function ConflictResolver({ version, rechoose }: { version: GraphAttempt; rechoose?: boolean }) {
  const a = version.attempt
  const data = useResource(`conflicts:${a.id}:${version.conflicts}`, () => api.conflicts(a.id))
  const files = data.data?.files ?? []
  return (
    <section className="conf-box" aria-label="Conflicts">
      <header>
        <b>
          {plural(version.conflicts, 'file')} changed on both sides
        </b>
        <span className="muted">Pick what each place should say.</span>
      </header>
      {data.error && <ErrorNote error={data.error} />}
      {!data.data && !data.error && <Skeleton rows={3} />}
      {files.map((f) => (
        <FileConflicts key={f.path} id={a.id} name={versionName(a)} file={f} rechoose={!!rechoose} />
      ))}
    </section>
  )
}

function FileConflicts({ id, name, file, rechoose }: { id: number; name: string; file: ConflictFile; rechoose: boolean }) {
  const toast = useToast()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const [editing, setEditing] = useState<number | null>(null)
  const [text, setText] = useState('')

  const choose = async (h: ConflictHunk, take: HunkTake, custom?: string) => {
    setBusy(true)
    try {
      const r = await api.resolve(id, file.path, [{ index: h.index, take, text: custom }])
      const left = r.resolve.open.length
      if (!left && rechoose) await api.promote(id)
      toast.ok(left ? `${file.path}: resolved. ${plural(left, 'file')} left.` : `All conflicts resolved. ${name} is ready.`, { undo: false })
      setEditing(null)
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }

  return (
    <div className="conf-file">
      <div className="conf-path mono">{file.path}</div>
      {file.hunks.map((h) => {
        const main = theirsName(h)
        return (
          <div key={h.index} className="conf-hunk">
            <div className="conf-sides">
              <div>
                <div className="conf-label">{main}</div>
                <pre className="conf-code">{h.theirs || ' '}</pre>
              </div>
              <div>
                <div className="conf-label">{name}</div>
                <pre className="conf-code">{h.ours || ' '}</pre>
              </div>
            </div>
            {editing === h.index ? (
              <div className="conf-edit">
                <textarea id={`conf-${file.path}-${h.index}`} value={text} onChange={(e) => setText(e.target.value)} rows={Math.max(3, text.split('\n').length + 1)} />
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
                  {main === 'main' ? WORDS.keepMain : WORDS.keep(main)}
                </button>
                <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => void choose(h, 'ours')}>
                  {WORDS.keep(name)}
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
