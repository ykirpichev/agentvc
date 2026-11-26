/* eslint-disable react-refresh/only-export-components -- the hook and its dialogs belong together */
// Review actions on one attempt: pick as winner (p), abandon with a reason
// (x), mark passed or failed (m p, m f), rebase onto the newer main (r b). Each writes through the daemon and
// refreshes every view at once; the events socket carries the same change
// to other open tabs.

import { useEffect, useRef, useState, type ReactNode } from 'react'
import { api, ApiError } from '../api/client'
import { useLive } from '../api/live'
import type { Attempt, RebaseResponse, Status } from '../api/types'
import { useHotkeys } from '../keyboard/hotkeys'
import { actorName } from '../lib/format'
import { Kbd, StatusPill } from './bits'
import { Icon } from './Icon'

const OPEN: Status[] = ['exploring', 'passed', 'failed']

export const isOpen = (a: Attempt | undefined) => !!a && OPEN.includes(a.status)

/** Open and promoted attempts can be rebased; the root attempt is the checkout. */
export const canRebase = (a: Attempt | undefined) =>
  !!a && a.id !== 1 && (isOpen(a) || a.status === 'promoted')

const QUICK_REASONS = [
  'Wrong approach',
  'Tests fail',
  'Change is too large',
  'Duplicates another attempt',
  'Superseded by a better attempt',
]

interface Toast {
  id: number
  kind: 'ok' | 'error'
  text: ReactNode
}

export interface AttemptActions {
  /** Dialogs and toasts; render once on the page. */
  node: ReactNode
  canAct: boolean
  promote: () => void
  abandon: () => void
  mark: (status: 'passed' | 'failed') => void
  /** Rebase onto the remote's default branch; conflicts open a dialog. */
  rebase: () => void
  canRebase: boolean
  /** Take whole files of `source`'s changes into the target (`agentvc take`). */
  take: (source: Attempt, paths: string[]) => void
  busy: boolean
}

/**
 * Bind review actions to `target`. `siblings` are the other attempts of the
 * same task, used to say which ones a promote will close.
 */
export function useAttemptActions(
  target: Attempt | undefined,
  siblings: Attempt[],
  opts: { group?: string; onDone?: () => void } = {},
): AttemptActions {
  const { refresh } = useLive()
  const [dialog, setDialog] = useState<
    | { kind: 'promote' | 'abandon'; attempt: Attempt }
    | { kind: 'conflict'; attempt: Attempt; paths: string[]; onto: string }
    | null
  >(null)
  const [busy, setBusy] = useState(false)
  const [toast, setToast] = useState<Toast | null>(null)
  const toastId = useRef(0)
  const canAct = isOpen(target) && !busy
  const group = opts.group ?? 'Review'

  const show = (kind: Toast['kind'], text: ReactNode) => setToast({ id: ++toastId.current, kind, text })

  const run = async (fn: () => Promise<unknown>, ok: ReactNode) => {
    setBusy(true)
    try {
      await fn()
      show('ok', ok)
      opts.onDone?.()
    } catch (e) {
      const err = e instanceof ApiError ? e : new ApiError(0, 'client', String(e))
      show('error', (
        <>
          <code>{err.code}</code> {err.message}
        </>
      ))
    } finally {
      setBusy(false)
      setDialog(null)
      refresh()
    }
  }

  const rebasable = canRebase(target) && !busy

  const rebased = (r: RebaseResponse) => {
    const x = r.rebase
    const at = `${x.onto_ref} (${x.onto.slice(0, 7)})`
    if (!x.rebased) return <>a{x.attempt} is already based on {at}</>
    if (x.conflicts.length > 0)
      return (
        <>
          a{x.attempt} rebased onto {at} with conflict markers in {x.conflicts.length}{' '}
          {x.conflicts.length === 1 ? 'file' : 'files'}; fix them in its view, then snapshot
        </>
      )
    return (
      <>
        a{x.attempt} rebased onto {at}: {x.changes.length} {x.changes.length === 1 ? 'file' : 'files'} updated
      </>
    )
  }

  const doRebase = async (attempt: Attempt, keepConflicts: boolean) => {
    setBusy(true)
    try {
      const r = await api.rebase(attempt.id, keepConflicts ? { keep_conflicts: true } : {})
      setDialog(null)
      show('ok', rebased(r))
      opts.onDone?.()
    } catch (e) {
      const err = e instanceof ApiError ? e : new ApiError(0, 'client', String(e))
      const paths = err.details.paths
      if (err.code === 'conflict' && Array.isArray(paths) && !keepConflicts) {
        setDialog({ kind: 'conflict', attempt, paths: paths.map(String), onto: String(err.details.onto ?? '') })
      } else {
        setDialog(null)
        show('error', (
          <>
            <code>{err.code}</code> {err.message}
          </>
        ))
      }
    } finally {
      setBusy(false)
      refresh()
    }
  }
  const rebase = () => {
    if (target && rebasable) void doRebase(target, false)
  }

  const promote = () => target && canAct && setDialog({ kind: 'promote', attempt: target })
  const abandon = () => target && canAct && setDialog({ kind: 'abandon', attempt: target })
  const mark = (status: 'passed' | 'failed') => {
    if (!target || !canAct || target.status === status) return
    const id = target.id
    void run(() => api.mark(id, status), <>a{id} marked {status}</>)
  }

  useHotkeys([
    { keys: 'p', description: 'Pick as winner', group, handler: promote },
    { keys: 'x', description: 'Abandon with a reason', group, handler: abandon },
    { keys: 'm p', description: 'Mark passed', group, handler: () => mark('passed') },
    { keys: 'm f', description: 'Mark failed', group, handler: () => mark('failed') },
    { keys: 'r b', description: 'Rebase onto the newer main', group, handler: rebase },
  ])

  const node = (
    <>
      {dialog?.kind === 'promote' && (
        <PromoteDialog
          attempt={dialog.attempt}
          losers={siblings.filter((s) => s.id !== dialog.attempt.id && isOpen(s))}
          busy={busy}
          onCancel={() => setDialog(null)}
          onConfirm={() => {
            const id = dialog.attempt.id
            void run(async () => {
              const r = await api.promote(id)
              return r
            }, <>a{id} picked as the winner</>)
          }}
        />
      )}
      {dialog?.kind === 'abandon' && (
        <AbandonDialog
          attempt={dialog.attempt}
          busy={busy}
          onCancel={() => setDialog(null)}
          onConfirm={(reason) => {
            const id = dialog.attempt.id
            void run(() => api.abandon(id, reason), <>a{id} abandoned</>)
          }}
        />
      )}
      {dialog?.kind === 'conflict' && (
        <ConflictDialog
          attempt={dialog.attempt}
          paths={dialog.paths}
          onto={dialog.onto}
          busy={busy}
          onCancel={() => setDialog(null)}
          onConfirm={() => void doRebase(dialog.attempt, true)}
        />
      )}
      {toast && <ToastNote key={toast.id} toast={toast} onClose={() => setToast(null)} />}
    </>
  )
  const take = (source: Attempt, paths: string[]) => {
    if (!target || !canAct) return
    const into = target
    const what = paths.length === 1 ? paths[0] : `${paths.length} files`
    void run(
      () => api.take(into.id, { source: `a${source.id}`, paths }),
      <>
        Took {what} from a{source.id} into a{into.id}
      </>,
    )
  }

  return { node, canAct, promote, abandon, mark, rebase, canRebase: rebasable, take, busy }
}

/** Primary and secondary action buttons for one attempt. */
export function ActionButtons({
  actions,
  compact,
}: {
  actions: AttemptActions
  compact?: boolean
}) {
  if (!actions.canAct && !actions.canRebase) return null
  return (
    <span className="action-buttons">
      {actions.canAct && (
        <>
          <button className="btn btn-win" onClick={actions.promote} disabled={actions.busy}>
            <Icon name="star" size={13} /> {compact ? 'Pick' : 'Pick winner'} <Kbd keys="p" />
          </button>
          <button className="btn btn-ghost" onClick={actions.abandon} disabled={actions.busy}>
            Abandon <Kbd keys="x" />
          </button>
        </>
      )}
      {actions.canRebase && !compact && <RebaseButton actions={actions} />}
    </span>
  )
}

/** Rebase onto the newer main; shown on its own where other actions are not. */
export function RebaseButton({ actions }: { actions: AttemptActions }) {
  if (!actions.canRebase) return null
  return (
    <button
      className="btn btn-ghost"
      onClick={actions.rebase}
      disabled={actions.busy}
      title="Merge the newer main into this attempt, so its PR carries only its own changes"
    >
      <Icon name="branch" size={13} /> {actions.busy ? 'Rebasing…' : 'Rebase onto main'} <Kbd keys="r b" />
    </button>
  )
}

function ToastNote({ toast, onClose }: { toast: Toast; onClose: () => void }) {
  useEffect(() => {
    const t = window.setTimeout(onClose, toast.kind === 'ok' ? 3500 : 8000)
    return () => window.clearTimeout(t)
  }, [toast, onClose])
  return (
    <div className={`toast toast-${toast.kind}`} role="status">
      <Icon name={toast.kind === 'ok' ? 'check' : 'x'} size={14} />
      <span>{toast.text}</span>
      <button className="icon-btn" onClick={onClose} aria-label="Dismiss">
        <Icon name="x" size={12} />
      </button>
    </div>
  )
}

function DialogFrame({
  title,
  onCancel,
  children,
  label,
}: {
  title: ReactNode
  label: string
  onCancel: () => void
  children: ReactNode
}) {
  return (
    <div className="overlay" onMouseDown={onCancel}>
      <div
        className="dialog action-dialog"
        role="dialog"
        aria-label={label}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="dialog-head">
          <h2>{title}</h2>
          <Kbd keys="esc" />
        </div>
        {children}
      </div>
    </div>
  )
}

function AttemptLine({ a }: { a: Attempt }) {
  return (
    <span className="attempt-line">
      <span className="row-id">a{a.id}</span> {actorName(a.actor)}
      {a.model && <span className="muted"> · {a.model}</span>}
      <StatusPill status={a.status} />
    </span>
  )
}

function PromoteDialog({
  attempt,
  losers,
  busy,
  onCancel,
  onConfirm,
}: {
  attempt: Attempt
  losers: Attempt[]
  busy: boolean
  onCancel: () => void
  onConfirm: () => void
}) {
  useHotkeys(
    [
      { keys: 'esc', description: 'Cancel', handler: onCancel, inInputs: true, hidden: true },
      { keys: ['enter', 'p'], description: 'Confirm', handler: () => !busy && onConfirm(), hidden: true },
    ],
    { exclusive: true },
  )
  return (
    <DialogFrame title="Pick the winner" label="Pick the winner" onCancel={onCancel}>
      <div className="dialog-body">
        <AttemptLine a={attempt} />
        {losers.length > 0 ? (
          <p className="muted">
            This closes {losers.length === 1 ? 'the other open attempt' : `the ${losers.length} other open attempts`} as{' '}
            <em>lost to a{attempt.id}</em>:
          </p>
        ) : (
          <p className="muted">No other open attempts on this task.</p>
        )}
        {losers.length > 0 && (
          <ul className="loser-list">
            {losers.map((l) => (
              <li key={l.id}>
                <AttemptLine a={l} />
              </li>
            ))}
          </ul>
        )}
      </div>
      <div className="dialog-foot">
        <button className="btn btn-ghost" onClick={onCancel}>
          Cancel
        </button>
        <button className="btn btn-win" onClick={onConfirm} disabled={busy} autoFocus>
          <Icon name="star" size={13} /> Pick a{attempt.id} <Kbd keys="enter" />
        </button>
      </div>
    </DialogFrame>
  )
}

function AbandonDialog({
  attempt,
  busy,
  onCancel,
  onConfirm,
}: {
  attempt: Attempt
  busy: boolean
  onCancel: () => void
  onConfirm: (reason: string) => void
}) {
  const [reason, setReason] = useState('')
  const input = useRef<HTMLTextAreaElement>(null)
  useEffect(() => input.current?.focus(), [])
  const submit = () => reason.trim() && !busy && onConfirm(reason.trim())
  useHotkeys(
    [
      { keys: 'esc', description: 'Cancel', handler: onCancel, inInputs: true, hidden: true },
      { keys: 'mod+enter', description: 'Abandon', handler: submit, inInputs: true, hidden: true },
    ],
    { exclusive: true },
  )
  return (
    <DialogFrame title="Abandon attempt" label="Abandon attempt" onCancel={onCancel}>
      <div className="dialog-body">
        <AttemptLine a={attempt} />
        <label className="field">
          <span>Why? The reason stays in history and shows on the attempt tree.</span>
          <textarea
            ref={input}
            rows={3}
            value={reason}
            placeholder="e.g. Rewrites the parser instead of fixing the bug"
            onChange={(e) => setReason(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey) {
                e.preventDefault()
                submit()
              }
            }}
          />
        </label>
        <div className="chips">
          {QUICK_REASONS.map((r) => (
            <button
              key={r}
              className={`chip ${reason === r ? 'chip-on' : ''}`}
              onClick={() => {
                setReason(r)
                input.current?.focus()
              }}
            >
              {r}
            </button>
          ))}
        </div>
      </div>
      <div className="dialog-foot">
        <span className="muted foot-hint">
          <Kbd keys="enter" /> abandon · <Kbd keys="shift+enter" /> new line
        </span>
        <button className="btn btn-ghost" onClick={onCancel}>
          Cancel
        </button>
        <button className="btn btn-danger" onClick={submit} disabled={busy || !reason.trim()}>
          Abandon a{attempt.id}
        </button>
      </div>
    </DialogFrame>
  )
}

function ConflictDialog({
  attempt,
  paths,
  onto,
  busy,
  onCancel,
  onConfirm,
}: {
  attempt: Attempt
  paths: string[]
  onto: string
  busy: boolean
  onCancel: () => void
  onConfirm: () => void
}) {
  useHotkeys(
    [
      { keys: 'esc', description: 'Cancel', handler: onCancel, inInputs: true, hidden: true },
      { keys: 'enter', description: 'Write conflict markers', handler: () => !busy && onConfirm(), hidden: true },
    ],
    { exclusive: true },
  )
  return (
    <DialogFrame title="Rebase conflicts" label="Rebase conflicts" onCancel={onCancel}>
      <div className="dialog-body">
        <AttemptLine a={attempt} />
        <p className="muted">
          Main{onto && <> (<code>{onto.slice(0, 7)}</code>)</>} changed the same lines in{' '}
          {paths.length === 1 ? 'this file' : `these ${paths.length} files`}. Nothing was changed.
        </p>
        <ul className="conflict-list">
          {paths.map((p) => (
            <li key={p}>
              <Icon name="warn" size={13} /> <code>{p}</code>
            </li>
          ))}
        </ul>
        <p className="muted">
          Write the files with conflict markers into a{attempt.id}'s view to resolve them there (it becomes{' '}
          <em>exploring</em>), or cancel and hand the conflict back to the agent.
        </p>
      </div>
      <div className="dialog-foot">
        <button className="btn btn-ghost" onClick={onCancel}>
          Cancel
        </button>
        <button className="btn btn-danger" onClick={onConfirm} disabled={busy} autoFocus>
          Write conflict markers <Kbd keys="enter" />
        </button>
      </div>
    </DialogFrame>
  )
}
