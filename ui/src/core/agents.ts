// Agents in plain words: one state each, shown as one dot and a few words,
// and the order of the "Needs you" list. Shared by every surface.

import type { Agent, AgentPermissions, AgentRunner, AgentSettings, AgentState } from '../api/types'

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

export const RUNNER_WORDS: Record<AgentRunner, string> = {
  claude: 'Claude Code',
  codex: 'Codex',
  cursor: 'Cursor',
  command: 'Custom command',
}
export const PERMISSION_WORDS: Record<AgentPermissions, string> = {
  auto: 'Auto: asks only when risky',
  edits: 'Edit files, ask before commands',
  ask: 'Ask before every change',
  bypass: 'Never ask (no checks)',
}

/** What each permission level means, for the permissions page. */
export const PERMISSION_HELP: Record<AgentPermissions, string> = {
  auto: "Claude Code's auto mode: a safety check approves routine steps; anything risky comes to you.",
  edits: 'Edits files on its own; asks you before every command it has no allow rule for.',
  ask: 'Asks you before every edit and every command your settings do not allow.',
  bypass: 'Never asks and skips the safety check. Only the guard rails below still apply.',
}

const PERMISSION_SHORT: Record<AgentPermissions, string> = {
  auto: 'auto mode',
  edits: 'asks before commands',
  ask: 'asks before every change',
  bypass: 'never asks',
}

/** The default when neither the workspace nor the repo sets one. */
export const DEFAULT_PERMISSIONS: AgentPermissions = 'auto'

/** Where the permissions page lives. */
export const PERMISSIONS_LINK = '/settings/agents'

/** One line saying how agents run in a workspace. */
export function settingsLine(s: AgentSettings | undefined): string {
  const runner = s?.runner ?? 'claude'
  const parts = [RUNNER_WORDS[runner]]
  if (s?.model) parts.push(s.model)
  if (s?.check) parts.push(`checks: ${s.check}`)
  if (runner === 'claude') parts.push(PERMISSION_SHORT[s?.permissions ?? DEFAULT_PERMISSIONS])
  if (runner === 'claude' && s?.sandbox) parts.push('sandboxed')
  if (s?.max) parts.push(`up to ${s.max} at once`)
  return parts.join(' · ')
}
