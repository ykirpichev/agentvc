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
}

export interface Snapshot {
  id: number
  attempt_id: number
  tree: string
  trigger: string
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
}

export type ChangeKind = 'added' | 'modified' | 'removed'

export interface FileChange {
  path: string
  kind: ChangeKind
}

export interface DiffResponse {
  a: Snapshot
  b: Snapshot
  changes: FileChange[]
}

export interface FileResponse {
  ref: string
  snapshot: number
  path: string
  mode: 'file' | 'executable' | 'symlink'
  oid: string
  size: number
  binary: boolean
  too_large: boolean
  content: string | null
}

/** `aN` (latest state of an attempt) or `sN` (one snapshot). */
export type Ref = `a${number}` | `s${number}`

export type DaemonEvent =
  | { type: 'hello'; workdir: string }
  | { type: 'task_created'; task: Task }
  | { type: 'attempt_created'; attempt: Attempt }
  | { type: 'attempt_updated'; attempt: Attempt; previous_status: Status }
  | { type: 'snapshot_created'; snapshot: Snapshot }
  | { type: 'resync' }

export interface ApiErrorBody {
  error: { code: string; message: string }
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
