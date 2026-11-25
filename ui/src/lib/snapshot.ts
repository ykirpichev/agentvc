import type { Snapshot } from '../api/types'

/** Category of a snapshot trigger, for coloring. */
export function triggerKind(t: string): string {
  if (t.startsWith('tool:')) return 'tool'
  if (t === 'run-exit') return 'run'
  if (['passed', 'failed', 'promoted'].includes(t)) return t
  return 'other'
}

/** What caused a snapshot, in words: the tool, a run's exit, a status change. */
export function triggerLabel(s: Snapshot): string {
  const t = s.trigger
  if (t.startsWith('tool:')) return t.slice(5)
  if (t === 'run-exit') {
    const code = s.meta?.['check_exit_code'] ?? s.meta?.['exit_code']
    return code === undefined || code === null ? 'Run finished' : `Run exited ${String(code)}`
  }
  if (t === 'fork') return 'Forked'
  if (t === 'stop') return 'Agent turn ended'
  return t.charAt(0).toUpperCase() + t.slice(1)
}
