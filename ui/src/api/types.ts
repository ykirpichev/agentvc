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
}

export type InboxReason = 'ready_to_pick' | 'failed'

export interface InboxItem {
  task: Task
  attempts: StatusCounts
  reasons: InboxReason[]
  best_check: 'passed' | 'failed' | null
  candidates: number[]
  cost_usd: number | null
  updated_at: Timestamp
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
