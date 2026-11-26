// The agentvc panel's state contract: what the hooks module keeps in
// `$.state`. A contract stands alone, so the daemon shapes it stores are copied
// here from the review UI's API types (ui/src/api/types.ts); the hooks module
// type-checks them against hooks/shared, which sync-shared.sh keeps equal to it.

// ---- daemon shapes (from ui/src/api/types.ts) ------------------------------

export type Status = 'exploring' | 'passed' | 'failed' | 'abandoned' | 'promoted'

/** Unix seconds. */
export type Timestamp = number

export interface Task {
  id: number
  goal: string
  criteria: string | null
  created_at: Timestamp
}

export interface StatusCounts {
  total: number
  exploring: number
  passed: number
  failed: number
  abandoned: number
  promoted: number
}

export interface TaskSummary extends Task {
  attempts: StatusCounts
  updated_at: Timestamp
}

export interface Attempt {
  id: number
  task_id: number | null
  parent_attempt: number | null
  parent_snapshot: number | null
  actor: string
  model: string | null
  prompt: string | null
  status: Status
  reason: string | null
  view_path: string
  backend: string
  created_at: Timestamp
  /** Names (`agentvc tag`) on this attempt's snapshots. */
  tags?: string[]
}

export interface TrunkCommit {
  commit: string
  summary: string
  time: Timestamp
  /** False for a base commit that is not on the upstream branch. */
  on_upstream: boolean
}

export interface GraphAttempt {
  attempt: Attempt
  base_commit: string | null
  /** Commits on the upstream its base lacks; null when unknown. */
  behind: number | null
  /** Conflicted files still open. */
  conflicts: number
  head_snapshot: number | null
  pr: { number: number; url: string; state: 'open' | 'closed' | 'merged' | null } | null
  /** Stacked onto another version: that version and the snapshot of it this one is on. */
  stacked_on?: { attempt: number; snapshot: number }
}

export interface Graph {
  /** Like `origin/main`, or `HEAD` without a remote. */
  upstream: string
  /** Newest first. */
  trunk: TrunkCommit[]
  attempts: GraphAttempt[]
}

export interface WsBranch {
  name: string
  workspace: string
  parent: string
  parent_kind: 'trunk' | 'stack' | 'other_window' | 'branch'
  parent_workspace: string | null
  parent_rev: string
  tip: string | null
  commits: number
  needs_restack: boolean
  behind: number
  conflicts: number
  current: boolean
  depth: number
  children: string[]
  pr: { number: number; state: string | null } | null
  attempt: number | null
  task: number | null
  status: string | null
}

export interface WsRequest {
  id: string
  workspace: string
  op: string
  branch?: string
  onto?: string
  new_name?: string
  from?: string
  created_at: number
  status: 'queued' | 'applied' | 'failed' | 'cancelled'
  reason?: string
  code?: string
  finished_at?: number
}

export interface WorkspaceInfo {
  id: string
  name: string
  kind: 'main' | 'worktree' | 'remote'
  machine: string
  path: string
  here: boolean
  state: 'idle' | 'agent_running'
  busy?: { since: number; seen: number; by: string | null }
  last_seen: number | null
  current: string | null
  trunk: string
  base_ref: string
  base: string | null
  pending: { branch: string; onto: string } | null
  branches: WsBranch[]
  requests: WsRequest[]
}

// ---- the panel's own state --------------------------------------------------

/** What the pane knows about the space, refreshed by polling agentvcd. */
export type AgentvcBoard = {
  isReachable: boolean
  error: string | null
  space: string | null
  graph: Graph | null
  tasks: TaskSummary[]
  /** Every window (worktree) with its stack; empty outside git. */
  workspaces: WorkspaceInfo[]
  /** The window this session works in, by its folder. */
  here: string | null
  /** Versions an agent is running in right now (`agentvc run`). */
  running: number[]
  fetchedAt: number
}

/** A row of the tree the person picked. */
export type AgentvcSel =
  | { kind: 'version'; id: number }
  | { kind: 'branch'; workspace: string; name: string }

/** What a rebase onto the picked target would do; nothing changed yet. */
export type AgentvcPreview = {
  attempt: number
  /** A commit, or `aN` to stack onto a version. */
  onto: string
  /** The target in words. */
  where: string
  isStack: boolean
  isUpdate: boolean
  clean: string[]
  clash: string[]
  error: string | null
}

export type AgentvcUi = {
  sel: AgentvcSel | null
  /** "Rebase onto…" / "Move onto…": waiting for a target to be pressed. */
  pick: AgentvcSel | null
  preview: AgentvcPreview | null
  showClosed: boolean
  /** The Discard reason field is open. */
  discarding: boolean
  busy: boolean
}

export type AgentvcHunk = {
  index: number
  ours: string
  theirs: string
  /** The other side in words: `main`, or a branch or version. */
  other: string
}

export type AgentvcFileDiff = {
  path: string
  kind: string
  /** Unified hunks, or null when there is nothing to draw (`note` says why). */
  diff: string | null
  note: string | null
}

/** The selected row's details, loaded when it is picked or changes. */
export type AgentvcDetail = {
  /** Which selection and state this was loaded for. */
  key: string
  isLoading: boolean
  error: string | null
  files: AgentvcFileDiff[]
  moreFiles: number
  /** A branch's own commits, newest first. */
  commits: { commit: string; subject: string }[]
  conflicts: { path: string; hunks: AgentvcHunk[] }[]
}

declare module 'claude-code' {
  interface PluginState {
    'agentvc-panel': {
      board: Shaped<AgentvcBoard>
      ui: Shaped<AgentvcUi>
      detail: Shaped<AgentvcDetail>
    }
  }
}
