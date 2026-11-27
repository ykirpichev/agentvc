// Agents in plain words: one state each, shown as one dot and a few words,
// and the order of the "Needs you" list. Shared by every surface.

import type { Agent, AgentState } from '../api/types'

export type AgentTone = 'ask' | 'bad' | 'ok' | 'run' | 'mute'

const STATES: Record<AgentState, { label: string; tone: AgentTone }> = {
  queued: { label: 'Waiting for a slot', tone: 'mute' },
  running: { label: 'Working', tone: 'run' },
  asking: { label: 'Has a question', tone: 'ask' },
  needs_approval: { label: 'Needs your OK', tone: 'ask' },
  ready: { label: 'Ready to review', tone: 'ok' },
  tests_failing: { label: 'Tests failing', tone: 'bad' },
  failed: { label: 'Failed', tone: 'bad' },
  stopped: { label: 'Stopped', tone: 'mute' },
  landed: { label: 'Landed', tone: 'mute' },
  discarded: { label: 'Discarded', tone: 'mute' },
}

/** The words and tone for an agent's state; a ready agent whose start moved says so. */
export function agentState(a: Pick<Agent, 'state' | 'stale'>): { label: string; tone: AgentTone } {
  const s = STATES[a.state]
  if (a.stale && (a.state === 'ready' || a.state === 'stopped')) return { label: `${s.label} · branch moved`, tone: s.tone }
  return s
}

/** Agents that wait on a person, most urgent first, then longest waiting. */
export function needsYou(agents: Agent[]): Agent[] {
  return agents
    .filter((a) => a.urgency !== null)
    .sort((x, y) => (x.urgency ?? 9) - (y.urgency ?? 9) || (x.waiting_since ?? 0) - (y.waiting_since ?? 0))
}

/** Agents doing something right now (or waiting for a slot). */
export const working = (agents: Agent[]) => agents.filter((a) => a.state === 'running' || a.state === 'queued')

/** Agents started from `branch` of workspace `ws` that are still open. */
export const agentsOn = (agents: Agent[], ws: string, branch: string) =>
  agents.filter((a) => a.workspace === ws && a.start.branch === branch && a.state !== 'landed' && a.state !== 'discarded')

export const agentLink = (id: string) => `/review/a/${encodeURIComponent(id)}`

/** Where its work goes, in words. */
export const resultWords = (a: Agent) => (a.mode === 'revise' ? `new version of ${a.start.branch}` : `new branch on ${a.start.branch}`)
