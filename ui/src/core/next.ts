// The one next step for a task, shown as the "Next:" pill and as the
// primary button. Order: choose a version, resolve conflicts, update from
// main, open the PR.

import type { GraphAttempt } from '../api/types'
import { versionName } from './words'

export type NextStep =
  | { kind: 'wait'; label: string }
  | { kind: 'choose'; label: string }
  | { kind: 'resolve'; label: string; version: number }
  | { kind: 'update'; label: string; version: number }
  | { kind: 'open-pr'; label: string; version: number }
  | { kind: 'done'; label: string; version: number; url: string }

export function nextStep(versions: GraphAttempt[]): NextStep {
  const chosen = versions.find((g) => g.attempt.status === 'promoted')
  if (!chosen) {
    const ready = versions.filter((g) => g.attempt.status === 'passed').length
    if (ready === 0 && versions.some((g) => g.attempt.status === 'exploring'))
      return { kind: 'wait', label: 'Agents are working' }
    return { kind: 'choose', label: 'Choose a version' }
  }
  const id = chosen.attempt.id
  if (chosen.conflicts > 0)
    return { kind: 'resolve', label: `Resolve ${chosen.conflicts} conflict${chosen.conflicts === 1 ? '' : 's'}`, version: id }
  if ((chosen.behind ?? 0) > 0) return { kind: 'update', label: `Update ${versionName(chosen.attempt)} from main`, version: id }
  if (chosen.pr && chosen.pr.state !== 'closed')
    return { kind: 'done', label: chosen.pr.state === 'merged' ? `PR #${chosen.pr.number} merged` : `PR #${chosen.pr.number} open`, version: id, url: chosen.pr.url }
  return { kind: 'open-pr', label: 'Open the PR', version: id }
}
