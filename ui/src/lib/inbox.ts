import type { TaskSummary } from '../api/types'

/** A task waits on a human when it has finished attempts and no winner yet. */
export function needsDecision(t: TaskSummary): boolean {
  return t.attempts.promoted === 0 && t.attempts.passed + t.attempts.failed > 0
}
