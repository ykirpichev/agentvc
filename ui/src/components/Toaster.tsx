/* eslint-disable react-refresh/only-export-components -- provider and hook belong together */
// One toast at a time, bottom centre. A toast for a change that went into
// the operation log offers Undo, which reverses the newest operation.

import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from 'react'
import { api, ApiError } from '../api/client'
import { useLive } from '../api/live'

interface Toast {
  id: number
  kind: 'ok' | 'error'
  text: ReactNode
  /** true: the space's operation log; a function: a custom undo that
   * returns what it undid (stack edits use their window's undo log). */
  undo: Undo
}

type Undo = boolean | (() => Promise<string>)

interface ToastApi {
  ok: (text: ReactNode, opts?: { undo?: Undo }) => void
  error: (e: unknown) => void
}

const Ctx = createContext<ToastApi>({ ok: () => {}, error: () => {} })

let nextId = 0

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toast, setToast] = useState<Toast | null>(null)
  const { refresh } = useLive()
  const ok = useCallback((text: ReactNode, opts?: { undo?: Undo }) => setToast({ id: ++nextId, kind: 'ok', text, undo: opts?.undo ?? false }), [])
  const error = useCallback((e: unknown) => {
    const err = e instanceof ApiError ? e : new ApiError(0, 'client', String(e))
    setToast({ id: ++nextId, kind: 'error', text: <><code>{err.code}</code> {err.message}</>, undo: false })
  }, [])

  useEffect(() => {
    if (!toast) return
    const t = window.setTimeout(() => setToast(null), toast.kind === 'ok' ? (toast.undo ? 7000 : 3500) : 9000)
    return () => window.clearTimeout(t)
  }, [toast])

  const undo = async (how: Undo) => {
    try {
      const what = typeof how === 'function' ? await how() : (await api.undo()).undid.summary
      setToast({ id: ++nextId, kind: 'ok', text: <>Undone: {what}</>, undo: false })
    } catch (e) {
      error(e)
    } finally {
      refresh()
    }
  }

  return (
    <Ctx.Provider value={{ ok, error }}>
      {children}
      {toast && (
        <div className={`gtoast gtoast-${toast.kind}`} role="status">
          <span>{toast.text}</span>
          {toast.undo && (
            <button type="button" onClick={() => void undo(toast.undo)}>
              Undo
            </button>
          )}
          <button type="button" className="gtoast-x" aria-label="Dismiss" onClick={() => setToast(null)}>
            ×
          </button>
        </div>
      )}
    </Ctx.Provider>
  )
}

export const useToast = () => useContext(Ctx)
