import type {
  ActionResponse,
  ApiErrorBody,
  AttemptStats,
  Blame,
  QueryParams,
  QueryRow,
  CompareResponse,
  InboxItem,
  LiveAttempt,
  StopResponse,
  Attempt,
  AttemptDetail,
  DiffResponse,
  FileResponse,
  RebaseOptions,
  RebaseResponse,
  Ref,
  Snapshot,
  SpaceInfo,
  TakeOptions,
  TakeResponse,
  TaskDetail,
  TaskSummary,
  ConflictsResponse,
  Graph,
  HunkTake,
  PrResponse,
  RebasePreview,
  ResolveResponse,
  UndoResponse,
  BranchDetail,
  WorkspaceList,
  WsConflicts,
  WsEditOutcome,
  WsFinish,
  WsResolve,
  WsOp,
  WsRequest,
  Agent,
  AgentList,
  AgentStep,
  StartAgent,
  AgentSettings,
  WorkspaceAgentSettings,
  PermissionsReport,
} from './types'
import { apiUrl } from '../core/base'

/** An error from the daemon, carrying its stable `code`. */
export class ApiError extends Error {
  readonly code: string
  readonly status: number
  /** Extra fields of the error object, e.g. `paths` for `conflict`. */
  readonly details: Record<string, unknown>
  constructor(status: number, code: string, message: string, details: Record<string, unknown> = {}) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.details = details
  }
}

async function get<T>(path: string, params?: Record<string, string | undefined>): Promise<T> {
  const qs = new URLSearchParams()
  for (const [k, v] of Object.entries(params ?? {})) if (v !== undefined) qs.set(k, v)
  return request<T>(`${apiUrl(path)}${qs.size ? `?${qs}` : ''}`)
}

async function post<T>(path: string, body?: unknown): Promise<T> {
  return request<T>(apiUrl(path), {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
}

async function request<T>(url: string, init: RequestInit = {}): Promise<T> {
  let res: Response
  try {
    res = await fetch(url, { ...init, headers: { accept: 'application/json', ...init.headers } })
  } catch (e) {
    throw new ApiError(0, 'offline', `cannot reach agentvcd (${String(e)})`)
  }
  const body: unknown = await res.json().catch(() => null)
  if (!res.ok) {
    const err = (body as ApiErrorBody | null)?.error
    const { code, message, ...details } = err ?? { code: 'http', message: res.statusText }
    throw new ApiError(res.status, code, message, details)
  }
  return body as T
}

export const api = {
  space: () => get<SpaceInfo>('/space'),
  tasks: () => get<TaskSummary[]>('/tasks'),
  task: (id: number) => get<TaskDetail>(`/tasks/${id}`),
  attempts: (task?: number) =>
    get<Attempt[]>('/attempts', { task: task === undefined ? undefined : String(task) }),
  attempt: (id: number) => get<AttemptDetail>(`/attempts/${id}`),
  snapshots: (attempt: number) => get<Snapshot[]>(`/attempts/${attempt}/snapshots`),
  snapshot: (id: number) => get<Snapshot>(`/snapshots/${id}`),
  /** Without `a`, diffs against the snapshot `b` was forked from. */
  diff: (b: Ref, a?: Ref) => get<DiffResponse>('/diff', { a, b }),
  file: (ref: Ref, path: string) => get<FileResponse>('/file', { ref, path }),
  taskStats: (task: number) => get<AttemptStats[]>(`/tasks/${task}/stats`),
  inbox: () => get<InboxItem[]>('/inbox'),
  compare: (attempts: number[]) =>
    get<CompareResponse>('/compare', { attempts: attempts.map((a) => `a${a}`).join(',') }),
  live: () => get<LiveAttempt[]>('/live'),
  stop: (id: number) => post<StopResponse>(`/attempts/${id}/stop`),
  promote: (id: number) => post<ActionResponse>(`/attempts/${id}/promote`),
  abandon: (id: number, reason: string) => post<ActionResponse>(`/attempts/${id}/abandon`, { reason }),
  /** Rebase onto `onto`, by default the remote's default branch (fetched). */
  rebase: (id: number, opts: RebaseOptions = {}) => post<RebaseResponse>(`/attempts/${id}/rebase`, opts),
  /** Take files (or hunks) of `opts.source`'s changes into attempt `id`. */
  take: (id: number, opts: TakeOptions) => post<TakeResponse>(`/attempts/${id}/take`, opts),
  /** Attempts matching every filter, oldest first. */
  query: (q: QueryParams) =>
    get<QueryRow[]>('/query', {
      task: q.task === undefined ? undefined : `t${q.task}`,
      status: q.status?.length ? q.status.join(',') : undefined,
      actor: q.actor || undefined,
      model: q.model || undefined,
      touched: q.touched?.length ? q.touched.join(',') : undefined,
      since: q.since || undefined,
      meta: q.meta?.length ? q.meta.join(',') : undefined,
      grep: q.grep || undefined,
    }),
  /** Who wrote each line of `path` in attempt `id`. */
  blame: (id: number, path: string, working = true) =>
    get<Blame>('/blame', { attempt: `a${id}`, path, working: String(working) }),
  /** Trunk commits and every attempt, for the branch graph. */
  graph: () => get<Graph>('/graph'),
  /** What a rebase onto `onto` would do; changes nothing. */
  rebasePreview: (id: number, onto?: string) =>
    post<{ rebase: RebasePreview }>(`/attempts/${id}/rebase`, { onto, preview: true }),
  conflicts: (id: number) => get<ConflictsResponse>(`/attempts/${id}/conflicts`),
  resolve: (id: number, path: string, hunks: { index: number; take: HunkTake; text?: string }[]) =>
    post<ResolveResponse>(`/attempts/${id}/conflicts/resolve`, { path, hunks }),
  /** Reverse `op`, or the newest undoable operation. */
  undo: (op?: number) => post<UndoResponse>('/undo', op === undefined ? {} : { op }),
  /** Export, push and open (or update) the attempt's PR. */
  openPr: (id: number, draft = false) => post<PrResponse>(`/attempts/${id}/pr`, { draft }),
  /** Every workspace (worktree) of the repo with its stack. */
  workspaces: () => get<WorkspaceList>('/workspaces'),
  /** Change a workspace's stack from here: applied now, or queued while its agent works. */
  stackEdit: (workspace: string, op: WsOp, keepConflicts = false) =>
    post<WsEditOutcome>(`/workspaces/${encodeURIComponent(workspace)}/stack`, keepConflicts ? { ...op, keep_conflicts: true } : op),
  /** One stacked branch: its own commits and files. */
  branch: (workspace: string, name: string) =>
    get<BranchDetail>(`/workspaces/${encodeURIComponent(workspace)}/branches/${encodeURIComponent(name)}`),
  /** The conflicts of a restack stopped in a workspace. */
  wsConflicts: (workspace: string) => get<WsConflicts>(`/workspaces/${encodeURIComponent(workspace)}/conflicts`),
  wsResolve: (workspace: string, path: string, hunks: { index: number; take: HunkTake; text?: string }[]) =>
    post<WsResolve>(`/workspaces/${encodeURIComponent(workspace)}/conflicts/resolve`, { path, hunks }),
  wsContinue: (workspace: string) => post<WsFinish>(`/workspaces/${encodeURIComponent(workspace)}/continue`),
  wsAbort: (workspace: string) => post<WsFinish>(`/workspaces/${encodeURIComponent(workspace)}/abort`),
  cancelRequest: (id: string) => post<{ request: WsRequest }>(`/requests/${encodeURIComponent(id)}/cancel`),
  /** Agents started on the workspaces' stacks (landed and discarded ones with `all`). */
  agents: (all = false) => get<AgentList>('/agents', { all: all ? '1' : undefined }),
  agent: (id: string) => get<{ agent: Agent; steps: AgentStep[] }>(`/agents/${encodeURIComponent(id)}`),
  startAgent: (body: StartAgent) => post<{ agent: Agent }>('/agents', body),
  agentReply: (id: string, text: string) => post<{ agent: Agent }>(`/agents/${encodeURIComponent(id)}/reply`, { text }),
  agentApprove: (id: string) => post<{ agent: Agent }>(`/agents/${encodeURIComponent(id)}/approve`),
  agentDeny: (id: string, reason?: string) => post<{ agent: Agent }>(`/agents/${encodeURIComponent(id)}/deny`, reason ? { reason } : {}),
  agentStop: (id: string) => post<{ agent: Agent }>(`/agents/${encodeURIComponent(id)}/stop`),
  agentLand: (id: string) => post<{ agent: Agent; edit: WsEditOutcome }>(`/agents/${encodeURIComponent(id)}/land`),
  agentDiscard: (id: string) => post<{ agent: Agent }>(`/agents/${encodeURIComponent(id)}/discard`),
  /** What agents may do: per workspace, per open agent, and agentvc's guard rails. */
  agentPermissions: () => get<PermissionsReport>('/agent-permissions'),
  agentSettings: (workspace: string) => get<WorkspaceAgentSettings>(`/workspaces/${encodeURIComponent(workspace)}/agent-settings`),
  setAgentSettings: (workspace: string, s: AgentSettings) =>
    post<WorkspaceAgentSettings>(`/workspaces/${encodeURIComponent(workspace)}/agent-settings`, s),
  mark: (id: number, status: 'passed' | 'failed' | 'exploring') =>
    post<ActionResponse>(`/attempts/${id}/mark`, { status }),
}

export const ref = {
  attempt: (id: number): Ref => `a${id}`,
  snapshot: (id: number): Ref => `s${id}`,
  /** A commit an attempt was rebased onto. */
  git: (commit: string): Ref => `git:${commit}`,
}
