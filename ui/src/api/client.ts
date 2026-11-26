import type {
  ActionResponse,
  ApiErrorBody,
  AttemptStats,
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
} from './types'

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
  return request<T>(`/api${path}${qs.size ? `?${qs}` : ''}`)
}

async function post<T>(path: string, body?: unknown): Promise<T> {
  return request<T>(`/api${path}`, {
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
  mark: (id: number, status: 'passed' | 'failed' | 'exploring') =>
    post<ActionResponse>(`/attempts/${id}/mark`, { status }),
}

export const ref = {
  attempt: (id: number): Ref => `a${id}`,
  snapshot: (id: number): Ref => `s${id}`,
  /** A commit an attempt was rebased onto. */
  git: (commit: string): Ref => `git:${commit}`,
}
