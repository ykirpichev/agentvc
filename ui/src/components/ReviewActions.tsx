/* eslint-disable react-refresh/only-export-components -- the hook and its dialogs belong together */
// Review actions on one attempt: pick as winner (p), abandon with a reason
// (x), mark passed or failed (m p, m f). Each writes through the daemon and
// refreshes every view at once; the events socket carries the same change
// to other open tabs.

import { useEffect, useRef, useState, type ReactNode } from 'react'
import { api, ApiError } from '../api/client'
import { useLive } from '../api/live'
import type { Attempt, Status } from '../api/types'
import { useHotkeys } from '../keyboard/hotkeys'
import { actorName } from '../lib/format'
import { Kbd, StatusPill } from './bits'
import { Icon } from './Icon'

const OPEN: Status[] = ['exploring', 'passed', 'failed']

export const isOpen = (a: Attempt | undefined) => !!a && OPEN.includes(a.status)

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
  const [dialog, setDialog] = useState<{ kind: 'promote' | 'abandon'; attempt: Attempt } | null>(null)
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
      {toast && <ToastNote key={toast.id} toast={toast} onClose={() => setToast(null)} />}
    </>
  )
  return { node, canAct, promote, abandon, mark, busy }
}

/** Primary and secondary action buttons for one attempt. */
export function ActionButtons({
  actions,
  compact,
}: {
  actions: AttemptActions
  compact?: boolean
}) {
  if (!actions.canAct) return null
  return (
    <span className="action-buttons">
      <button className="btn btn-win" onClick={actions.promote} disabled={actions.busy}>
        <Icon name="star" size={13} /> {compact ? 'Pick' : 'Pick winner'} <Kbd keys="p" />
      </button>
      <button className="btn btn-ghost" onClick={actions.abandon} disabled={actions.busy}>
        Abandon <Kbd keys="x" />
      </button>
    </span>
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
