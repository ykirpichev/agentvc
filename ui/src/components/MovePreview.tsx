// Before a version moves onto another commit: what merges cleanly, what was
// resolved automatically, and which files will need a choice. Nothing
// changes until Rebase is pressed, and Undo reverses it.

import { useState } from 'react'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { GraphAttempt, TrunkCommit } from '../api/types'
import { useResource } from '../api/useResource'
import { shortSha, versionName } from '../core/words'
import { plural } from '../lib/format'
import { ErrorNote, Skeleton } from './bits'
import { useToast } from './Toaster'

export function MovePreview({
  version,
  onto,
  target,
  onDone,
  onCancel,
}: {
  version: GraphAttempt
  onto: string
  target: TrunkCommit | undefined
  onDone: () => void
  onCancel: () => void
}) {
  const a = version.attempt
  const preview = useResource(`preview:${a.id}:${onto}`, () => api.rebasePreview(a.id, onto))
  const toast = useToast()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const where = target ? `${target.summary} (${shortSha(onto)})` : shortSha(onto)
  const p = preview.data?.rebase
  const conflicts = p?.conflict_files.filter((f) => f.remaining > 0) ?? []
  const auto = p?.resolved ?? []
  const touched = new Set([...conflicts.map((f) => f.path), ...auto.map((f) => f.path)])
  const clean = (p?.changes ?? []).filter((c) => !touched.has(c.path))

  const go = async () => {
    setBusy(true)
    try {
      await api.rebase(a.id, { onto, keep_conflicts: conflicts.length > 0, no_fetch: true })
      toast.ok(
        conflicts.length
          ? `Rebased ${versionName(a)} onto ${target?.summary ?? shortSha(onto)}. ${plural(conflicts.length, 'file')} to resolve below.`
          : `Rebased ${versionName(a)} onto ${target?.summary ?? shortSha(onto)}.`,
        { undo: true },
      )
      onDone()
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }

  return (
    <section className="move-card" aria-label="Rebase preview">
      <header>
        <b>
          Rebase {versionName(a)} onto {where}?
        </b>
        <span className="tone tone-acc">preview</span>
      </header>
      {preview.error && <ErrorNote error={preview.error} />}
      {!p && !preview.error && <Skeleton rows={2} />}
      {p && (
        <ul>
          {!p.rebased && p.changes.length === 0 && conflicts.length === 0 && <li>Already based on this commit; nothing to do.</li>}
          {clean.length > 0 && <li>✓ {plural(clean.length, 'file')} from main merge in cleanly</li>}
          {auto.length > 0 && <li>✓ {plural(auto.length, 'file')} changed on both sides, resolved automatically: {auto.map((f) => f.path).join(', ')}</li>}
          {conflicts.map((f) => (
            <li key={f.path}>
              ⚠ <span className="mono">{f.path}</span>: {plural(f.remaining, 'place')} changed on both sides. You pick what to keep right after.
            </li>
          ))}
          <li className="muted">Nothing changes until you press Rebase, and Undo puts it back.</li>
        </ul>
      )}
      <footer>
        <button type="button" className="btn btn-primary" disabled={busy || !p} onClick={() => void go()}>
          {conflicts.length ? `Rebase and resolve ${conflicts.length}` : 'Rebase'}
        </button>
        <button type="button" className="btn btn-ghost" onClick={onCancel}>
          Cancel
        </button>
      </footer>
    </section>
  )
}
