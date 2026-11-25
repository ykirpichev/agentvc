// The agentvc panel's state contract: what the hooks module keeps in
// `$.state` and the daemon shapes it reads (a subset of agentvcd's JSON API).

export type AgentvcStatus =
  | 'exploring'
  | 'passed'
  | 'failed'
  | 'abandoned'
  | 'promoted'

export type AgentvcAttempt = {
  id: number
  task_id: number | null
  parent_attempt: number | null
  actor: string
  model: string | null
  prompt: string | null
  status: AgentvcStatus
  reason: string | null
  created_at: number
}

export type AgentvcCounts = {
  total: number
  exploring: number
  passed: number
  failed: number
  abandoned: number
  promoted: number
}

export type AgentvcTask = {
  id: number
  goal: string
  created_at: number
  updated_at: number
  attempts: AgentvcCounts
}

/** One open attempt from `/api/live`, cut down to what the panel draws. */
export type AgentvcLive = {
  attempt: number
  task: number | null
  goal: string | null
  model: string | null
  /** The latest snapshot's trigger: the agent's current step. */
  step: string | null
  files: number
  overlaps: number
  isRunning: boolean
  lastActivity: number
  logTail: string | null
}

/** Per-attempt numbers from `/api/tasks/{id}/stats`. */
export type AgentvcStats = {
  cost: number | null
  summary: string | null
  check: boolean | null
}

export type AgentvcBoard = {
  isReachable: boolean
  error: string | null
  space: string | null
  tasks: AgentvcTask[]
  attempts: AgentvcAttempt[]
  live: AgentvcLive[]
  /** Task ids the inbox says need a decision. */
  inbox: number[]
  stats: Record<string, AgentvcStats>
  fetchedAt: number
}

declare module 'claude-code' {
  interface PluginState {
    'agentvc-panel': {
      board: Shaped<AgentvcBoard>
      expanded: Shaped<number[]>
      showDone: boolean
    }
  }
}
