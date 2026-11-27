// Stack edits from the branch tree: run one in a window and say what
// happened, and the drag payload of a branch row.

import { useState, type DragEvent } from 'react'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { WorkspaceInfo, WsOp } from '../api/types'
import { branchLink, useGo } from '../core/links'
import { useToast } from './Toaster'

/** Drag payload of a branch row: `{ workspace, branch }` as JSON. */
export const BRANCH_MIME = 'application/x-agentvc-branch'

/** A dragged branch, read while dragging (types only) or on drop (data). */
export const isBranchDrag = (e: DragEvent) => e.dataTransfer.types.includes(BRANCH_MIME)
export function branchDrop(e: DragEvent): { workspace: string; branch: string } | null {
  try {
    const v = JSON.parse(e.dataTransfer.getData(BRANCH_MIME)) as { workspace?: unknown; branch?: unknown }
    return typeof v.workspace === 'string' && typeof v.branch === 'string' ? { workspace: v.workspace, branch: v.branch } : null
  } catch {
    return null
  }
}

/** Undo for a toast: the window's latest stack command (`stack undo`). */
export const undoIn = (workspace: string, done: string) => async () => {
  await api.stackEdit(workspace, { op: 'undo' })
  return done
}

/** Run a stack edit in a window and say what happened. */
export function useStackEdit() {
  const toast = useToast()
  const go = useGo()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const run = async (w: WorkspaceInfo, op: WsOp, done: string, keep = false) => {
    setBusy(true)
    try {
      const r = await api.stackEdit(w.id, op, keep)
      const stopped = r.result?.conflict?.branch
      if (r.outcome === 'queued') toast.ok(`Queued for ${w.name}: ${r.reason ?? 'an agent is working there'}. It applies when the agent stops.`, { undo: false })
      else if (stopped) {
        toast.ok(`${stopped} clashes with its parent. Pick what to keep.`, { undo: false })
        go(branchLink(w.id, stopped))
      } else toast.ok(done, { undo: op.op === 'checkout' ? false : undoIn(w.id, done) })
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }
  return { run, busy }
}
