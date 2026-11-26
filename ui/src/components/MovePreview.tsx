// Before a version moves onto another commit, or is stacked on top of another
// version (`onto` is then `aN`): what merges cleanly, what was
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
  targetVersion,
  onDone,
  onCancel,
}: {
  version: GraphAttempt
  onto: string
  target: TrunkCommit | undefined
  /** Set when stacking onto a version. */
  targetVersion?: GraphAttempt
  onDone: () => void
  onCancel: () => void
}) {
  const a = version.attempt
  const preview = useResource(`preview:${a.id}:${onto}`, () => api.rebasePreview(a.id, onto))
  const toast = useToast()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const where = targetVersion
    ? versionName(targetVersion.attempt)
    : target
      ? `${target.summary} (${shortSha(onto)})`
      : shortSha(onto)
  const short = targetVersion ? where : (target?.summary ?? shortSha(onto))
  const updating = !!targetVersion && version.stacked_on?.attempt === targetVersion.attempt.id
  const verb = updating ? 'Update' : targetVersion ? 'Stack' : 'Rebase'
  const done = !targetVersion
    ? `Rebased ${versionName(a)} onto ${short}`
    : updating
      ? `Updated ${versionName(a)} from ${short}`
      : `Stacked ${versionName(a)} on ${short}`
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
        conflicts.length ? `${done}. ${plural(conflicts.length, 'file')} to resolve below.` : `${done}.`,
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
          {updating ? (
            <>
              Update {versionName(a)} from {where}&apos;s latest state?
            </>
          ) : targetVersion ? (
            <>
              Stack {versionName(a)} on top of {where}?
            </>
          ) : (
            <>
              Rebase {versionName(a)} onto {where}?
            </>
          )}
        </b>
        <span className="tone tone-acc">preview</span>
      </header>
      {preview.error && <ErrorNote error={preview.error} />}
      {!p && !preview.error && <Skeleton rows={2} />}
      {p && (
        <ul>
          {!p.rebased && p.changes.length === 0 && conflicts.length === 0 && <li>Already based on this {targetVersion ? 'version' : 'commit'}; nothing to do.</li>}
          {clean.length > 0 && (
            <li>
              ✓ {plural(clean.length, 'file')} from {targetVersion ? short : 'main'} merge in cleanly
            </li>
          )}
          {targetVersion && !updating && <li className="muted">{versionName(a)} then shows only its own changes, and Update from {short} follows it when {short} changes.</li>}
          {auto.length > 0 && <li>✓ {plural(auto.length, 'file')} changed on both sides, resolved automatically: {auto.map((f) => f.path).join(', ')}</li>}
          {conflicts.map((f) => (
            <li key={f.path}>
              ⚠ <span className="mono">{f.path}</span>: {plural(f.remaining, 'place')} changed on both sides. You pick what to keep right after.
            </li>
          ))}
          <li className="muted">Nothing changes until you press {verb}, and Undo puts it back.</li>
        </ul>
      )}
      <footer>
        <button type="button" className="btn btn-primary" disabled={busy || !p} onClick={() => void go()}>
          {conflicts.length ? `${verb} and resolve ${conflicts.length}` : verb}
        </button>
        <button type="button" className="btn btn-ghost" onClick={onCancel}>
          Cancel
        </button>
      </footer>
    </section>
  )
}
