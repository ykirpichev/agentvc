// Types mirroring the JSON served by `agentvcd` (crates/agentvcd/src/api.rs
// and agentvc-core). Keep in sync by hand: the API is small and stable.

export type Status = 'exploring' | 'passed' | 'failed' | 'abandoned' | 'promoted'

export const STATUSES: readonly Status[] = [
  'exploring',
  'passed',
  'failed',
  'promoted',
  'abandoned',
]

/** Unix seconds. */
export type Timestamp = number

export interface SpaceInfo {
  workdir: string
  name: string
  format_version: number
  daemon_version: string
}

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

export interface Snapshot {
  id: number
  attempt_id: number
  tree: string
  trigger: string
  /** Names (`agentvc tag`) on this snapshot. */
  tags?: string[]
  meta: Record<string, unknown> | null
  created_at: Timestamp
}

export interface TaskDetail {
  task: Task
  attempts: Attempt[]
}

export interface AttemptDetail {
  attempt: Attempt
  head: Snapshot | null
  snapshot_count: number
  children: number[]
  /** The attempt's own latest rebase; its diffs compare against this commit. */
  git_base: GitBase | null
  /** The git commit the head is based on (what `agentvc pr` parents on). */
  base_commit: string | null
  /** Its pull request, if `agentvc pr` opened one. */
  pr_url?: string | null
  /** The local branch `agentvc land` put it on. */
  landed?: Landing | null
  /** GitHub's checks, reviews and comment threads on its PR, as of the last refresh. */
  pr?: PrFeedback | null
}

/** What GitHub said about a PR at the last `agentvc pr --refresh`. */
export interface PrFeedback {
  url: string
  number: number
  state: 'open' | 'closed' | 'merged'
  draft: boolean
  merge_commit: string | null
  checks: {
    state: 'pass' | 'fail' | 'pending' | 'none'
    passed: number
    failed: number
    pending: number
    failing: string[]
    running: string[]
  }
  reviews: { author: string; state: string; body: string }[]
  decision: 'approved' | 'changes_requested' | 'none'
  threads: {
    path: string
    line: number | null
    resolved: boolean | null
    outdated: boolean
    comments: { author: string; body: string; url: string | null }[]
  }[]
  unresolved: number
  refreshed_at: Timestamp
}

/** Where `agentvc land` put an attempt. */
export interface Landing {
  branch: string
  commit: string
  mode: string
  created_at: Timestamp
}

/** A commit an attempt was rebased onto. */
export interface GitBase {
  attempt_id: number
  snapshot_id: number
  commit: string
  tree: string
  created_at: Timestamp
}

export type ChangeKind = 'added' | 'modified' | 'removed'

export interface FileChange {
  path: string
  kind: ChangeKind
}

export interface DiffResponse {
  /** Null when the base is a git commit (`a_ref` is then `git:<commit>`). */
  a: Snapshot | null
  b: Snapshot
  a_ref: Ref
  b_ref: Ref
  changes: FileChange[]
}

export interface FileResponse {
  ref: string
  snapshot: number | null
  path: string
  mode: 'file' | 'executable' | 'symlink'
  oid: string
  size: number
  binary: boolean
  too_large: boolean
  content: string | null
}

/**
 * `aN` (latest state of an attempt), `sN` (one snapshot), or `git:<commit>`
 * (a commit an attempt was rebased onto).
 */
export type Ref = `a${number}` | `s${number}` | `git:${string}`

export type DaemonEvent =
  | { type: 'hello'; workdir: string }
  | { type: 'task_created'; task: Task }
  | { type: 'attempt_created'; attempt: Attempt }
  | { type: 'attempt_updated'; attempt: Attempt; previous_status: Status }
  | { type: 'snapshot_created'; snapshot: Snapshot }
  | { type: 'agent_changed'; agent: string; state: AgentState }
  | { type: 'resync' }

export interface ApiErrorBody {
  /** Some codes carry more fields, e.g. `conflict` has `paths` and `onto`. */
  error: { code: string; message: string; [key: string]: unknown }
}

// ---- review endpoints (crates/agentvcd/src/review.rs) ----------------------

export interface CheckResult {
  passed: boolean
  exit_code: number | null
  snapshot: number
  /** The check's name (`tests` unless named otherwise); absent for old run checks. */
  name?: string
  /** Recorded on an older state than the version's latest (e.g. before an update from main). */
  stale?: boolean
  /** `pass · 120 passed · 2m 03s`. */
  summary?: string
}

/** What `agentvc annotate` (and the Claude Code hooks) recorded about the agent. */
export interface AgentSummary {
  models: string[]
  cost_usd: number | null
  tokens_in: number | null
  tokens_out: number | null
  notes: { text: string; actor: string; created_at: Timestamp }[]
  /** Paths or URLs. */
  transcripts: string[]
  annotations: number
}

export interface AttemptStats {
  attempt: number
  snapshots: number
  check: CheckResult | null
  last_exit_code: number | null
  cost_usd: number | null
  tokens: number | null
  /** Seconds from creation to the latest snapshot. */
  duration_s: number
  run_ms: number | null
  summary: string | null
  last_activity: Timestamp
  agent?: AgentSummary
}

export type InboxReason = 'ready_to_pick' | 'failed' | 'checks_failed' | 'changes_requested'

export interface InboxItem {
  task: Task
  attempts: StatusCounts
  reasons: InboxReason[]
  best_check: 'passed' | 'failed' | null
  candidates: number[]
  cost_usd: number | null
  updated_at: Timestamp
  /** The promoted attempt's PR feedback, for `checks_failed` and `changes_requested`. */
  pr?: PrFeedback | null
}

export type CellKind = 'unchanged' | 'added' | 'modified' | 'removed'

export interface CompareCell {
  kind: CellKind
  oid: string | null
  added: number
  removed: number
  binary: boolean
}

export interface CompareFile {
  path: string
  /** Every attempt has the same content here. */
  same: boolean
  cells: CompareCell[]
}

export interface CompareColumn {
  attempt: Attempt
  head: Snapshot
  stats: AttemptStats
  files_changed: number
  lines_added: number
  lines_removed: number
}

export interface CompareResponse {
  base: Snapshot
  columns: CompareColumn[]
  files: CompareFile[]
}

export interface ActionResponse {
  attempt: Attempt
  closed_siblings: number[]
}

// ---- live board (crates/agentvcd/src/live.rs) -----------------------------

export interface LiveRun {
  id: number
  command: string[]
  check_command: string | null
  started_at: Timestamp
  alive: boolean
  log_tail: string[]
}

export interface LiveAttempt {
  attempt: Attempt
  task_goal: string | null
  head: Snapshot | null
  stats: AttemptStats
  files_changed: number
  files: FileChange[]
  /** Path → other running attempts that changed it too. */
  overlaps: Record<string, number[]>
  run: LiveRun | null
}

export interface StopResponse {
  run: number
  pid: number
  signal: string
}

// ---- rebase (crates/agentvcd/src/rebase.rs) -------------------------------

export interface RebaseResult {
  attempt: number
  onto: string
  /** What `onto` was resolved from, e.g. `origin/main`. */
  onto_ref: string
  from: string | null
  /** False when the attempt was already based on `onto`. */
  rebased: boolean
  snapshot: Snapshot | null
  changes: FileChange[]
  /** Files left with conflict markers (only with keep_conflicts). */
  conflicts: string[]
  view_updated: boolean
  status: Status
}

export interface RebaseResponse {
  rebase: RebaseResult
  attempt: Attempt
}

export interface TakeOptions {
  /** The attempt to take from, as aN. */
  source: string
  /** Changed files, directories or globs to take whole. */
  paths?: string[]
  /** Single hunks as FILE:N, numbered from 1. */
  hunks?: string[]
  keep_conflicts?: boolean
}

export interface TakeResult {
  into: number
  source: number
  source_snapshot: number
  mode: 'take' | 'merge'
  taken: { path: string; kind: 'added' | 'modified' | 'removed' }[]
  snapshot: Snapshot | null
  changes: { path: string; kind: 'added' | 'modified' | 'removed' }[]
  conflicts: string[]
  status: Status
}

export interface TakeResponse {
  take: TakeResult
  attempt: Attempt
}

export interface RebaseOptions {
  onto?: string
  keep_conflicts?: boolean
  no_fetch?: boolean
}

// ---- history queries (crates/agentvc-core/src/query.rs, blame.rs) ---------

/** Filters for `GET /api/query`; every field narrows the result (AND). */
export interface QueryParams {
  task?: number
  status?: Status[]
  /** Substring of the actor, e.g. `claude`. */
  actor?: string
  model?: string
  /** Paths, directories or globs the attempt changed (any of them). */
  touched?: string[]
  /** `2h`, `3d`, `1w`, `YYYY-MM-DD` or Unix seconds. */
  since?: string
  /** KEY=VALUE pairs on the attempt's snapshot meta (all of them). */
  meta?: string[]
  /** Substring of the prompt or reason. */
  grep?: string
}

/** One attempt matching a query (`agentvc log --flat --json`). */
export interface QueryRow {
  id: number
  task_id: number | null
  task: string | null
  parent_attempt: number | null
  actor: string
  model: string | null
  status: Status
  reason: string | null
  created_at: Timestamp
  snapshots: number
  last_snapshot_at: Timestamp | null
  meta: Record<string, string>
  /** Files it changed that match `touched`; absent without that filter. */
  touched?: string[] | null
}

export type BlameOrigin =
  | {
      kind: 'snapshot'
      snapshot: number
      attempt: number
      actor: string
      model: string | null
      trigger: string
      created_at: Timestamp
      /** For a `take`/`merge` snapshot: the attempt it came from, as aN. */
      source?: string
    }
  | { kind: 'git'; commit: string; author: string; time: Timestamp; summary: string }
  | { kind: 'working'; attempt: number }

export interface BlameLine {
  line: number
  text: string
  origin: BlameOrigin
}

export interface Blame {
  attempt: number
  path: string
  snapshot: number | null
  working: boolean
  lines: BlameLine[]
}

// ---- branch graph (crates/agentvcd/src/graph.rs) ---------------------------

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
  /** The latest recorded test result (`agentvc check`, or `run --check`). */
  tests?: { name: string; passed: boolean; stale: boolean; snapshot: number; summary: string }
  /** Model, cost, tokens, notes and transcripts (`agentvc annotate`). */
  agent?: AgentSummary
}

export interface Graph {
  /** Like `origin/main`, or `HEAD` without a remote. */
  upstream: string
  /** Newest first. */
  trunk: TrunkCommit[]
  attempts: GraphAttempt[]
}

export interface UndoResponse {
  op: number
  undid: { id: number; kind: string; summary: string }
  warnings: string[]
}

export interface PrResponse {
  pr: { number: number; url: string; updated: boolean; base: string; repo: string }
  attempt: Attempt
}

// ---- conflicts (crates/agentvcd/src/conflicts.rs) --------------------------

export interface ConflictHunk {
  index: number
  line: number
  ours: string
  base: string | null
  theirs: string
  ours_label: string
  theirs_label: string
}

export interface ConflictFile {
  path: string
  hunks: ConflictHunk[]
  text: string
}

export interface ConflictsResponse {
  attempt: Attempt
  conflicts: { id: number; path: string; operation: string; theirs_label: string | null }[]
  files: ConflictFile[]
}

export type HunkTake = 'ours' | 'theirs' | 'both' | 'both-theirs-first' | 'base' | 'custom'

export interface ResolveResponse {
  resolve: { path: string; resolved_hunks: number; remaining_hunks: number; open: string[] }
  attempt: Attempt
}

/** What a rebase would do (`preview: true`); nothing is changed. */
export interface RebasePreview extends RebaseResult {
  resolved: { path: string; strategy: string; hunks: number; remaining: number }[]
  conflict_files: { path: string; strategy: string; hunks: number; remaining: number }[]
  preview: boolean
}

// ---- workspaces (every worktree's stack; docs/stacks.md "Workspaces") ------

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
  pr: { number: number; state: string | null; status?: PrBrief } | null
  attempt: number | null
  task: number | null
  status: string | null
}

/** A stack PR's checks and review as last fetched (`stack log --prs`, the daemon's poller). */
export interface PrBrief {
  checks: 'pass' | 'fail' | 'pending' | 'none'
  failing?: string[]
  review: 'approved' | 'changes_requested' | 'none'
  unresolved: number
  mergeable: boolean | null
  mergeable_state?: string
  head: string | null
  at: number
}

export type WsOp =
  | { op: 'move'; branch: string; onto: string }
  | { op: 'restack'; branch?: string }
  | { op: 'rename'; branch: string; new_name: string }
  | { op: 'delete'; branch: string }
  | { op: 'checkout'; branch: string }
  | { op: 'undo' }
  | { op: 'merge'; branch: string }
  | { op: 'sync' }
  | { op: 'submit' }

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
  /** How agents started here run; empty fields fall back to the repo's defaults. */
  agent_settings?: AgentSettings
}

export interface WorkspaceList {
  here: string | null
  workspaces: WorkspaceInfo[]
}

export interface WsEditOutcome {
  outcome: 'applied' | 'queued'
  request_id: string
  workspace: string
  reason?: string
  /** With `keep_conflicts`, a restack that stopped says where. */
  result?: { conflict?: { branch: string; paths: string[] } | null; restack?: { conflict?: { branch: string; paths: string[] } | null }; merged?: { branch: string; number: number }[]; pushed?: string[]; prs?: { branch: string; number: number; url: string }[]; stopped?: { branch: string; code: string; message: string } | null }
}

/** A restack stopped on a conflict in a workspace. Hunks are from the branch's side: `ours` is the branch. */
export interface WsConflicts {
  workspace: string
  stopped: boolean
  branch: string | null
  parent: string | null
  onto: string | null
  subject: string | null
  files: { path: string; hunks: ConflictHunk[]; text: string }[]
}

export interface WsResolve {
  path: string
  remaining_hunks: number
  open: string[]
}

export interface WsFinish {
  outcome: 'continued' | 'stopped' | 'aborted'
  conflicts: WsConflicts | null
}

export interface BranchDetail {
  workspace: string
  name: string
  parent: string
  base: string
  tip: string
  current: boolean
  commits: { commit: string; subject: string; author: string; time: number }[]
  files: { path: string; kind: ChangeKind }[]
}

// ---- agents (crates/agentvcd/src/agents.rs) ---------------------------------

export type AgentState =
  | 'queued'
  | 'running'
  | 'asking'
  | 'needs_approval'
  | 'ready'
  | 'tests_failing'
  | 'failed'
  | 'stopped'
  | 'landed'
  | 'discarded'

export interface AgentQuestion {
  id: string
  kind: 'ask' | 'approve'
  text: string
  tool?: string
  input?: unknown
  asked_at: number
}

export interface Agent {
  id: string
  title: string
  prompt: string
  workspace: string
  workspace_name: string
  start: { branch: string; commit: string; trunk: boolean }
  mode: 'new' | 'revise'
  branch: string
  worktree: string
  runner: string
  permissions?: AgentPermissions
  model?: string
  check?: string
  machine: string
  state: AgentState
  created_at: number
  updated_at: number
  started_at: number | null
  finished_at: number | null
  seconds: number
  cost_usd: number
  turns: number
  last_step: string | null
  question: AgentQuestion | null
  tests: { command: string; exit_code: number | null; passed: boolean; tail: string[]; duration_ms: number } | null
  result: { commit: string | null; files: number; insertions: number; deletions: number; paths: string[]; changes?: { path: string; kind: ChangeKind }[] } | null
  error: string | null
  landed: { outcome: 'applied' | 'queued'; request_id: string; branch: string; at: number } | null
  messages: { from: 'you' | 'agent'; text: string; at: number }[]
  stale: boolean
  /** Rank in Needs you, 1 most urgent; null when it does not need you. */
  urgency: number | null
  waiting_since: number | null
}

export interface AgentList {
  agents: Agent[]
  needs_you: number
  working: number
  limit: number
  machine: string
  /** False in the hosted UI when that machine's daemon runs without --allow-remote-agents: agents can be watched, not started or answered. */
  can_steer: boolean
}

export interface AgentStep {
  at: number
  kind: 'text' | 'tool' | 'you' | 'question' | 'result' | 'check' | 'line'
  text: string
}

export interface StartAgent {
  workspace: string
  from: string
  at?: string
  prompt: string
  mode: 'new' | 'revise'
  model?: string
  check?: string
  runner?: AgentRunner
  permissions?: AgentPermissions
}

export type AgentRunner = 'claude' | 'codex' | 'cursor' | 'command'
export type AgentPermissions = 'edits' | 'ask' | 'auto'

/** `agentvc workspace agents`: how agents started in a workspace run. */
export interface AgentSettings {
  runner?: AgentRunner
  command?: string
  model?: string
  check?: string
  permissions?: AgentPermissions
  max?: number
}

export interface WorkspaceAgentSettings {
  workspace: string
  workspace_name: string
  settings: AgentSettings
  /** Repo defaults (git config agentvc.agent-*) under the workspace's. */
  defaults: AgentSettings
  machine_limit: number
}
