import type {
  ApiErrorBody,
  Attempt,
  AttemptDetail,
  DiffResponse,
  FileResponse,
  Ref,
  Snapshot,
  SpaceInfo,
  TaskDetail,
  TaskSummary,
} from './types'

/** An error from the daemon, carrying its stable `code`. */
export class ApiError extends Error {
  readonly code: string
  readonly status: number
  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }
}

async function get<T>(path: string, params?: Record<string, string | undefined>): Promise<T> {
  const qs = new URLSearchParams()
  for (const [k, v] of Object.entries(params ?? {})) if (v !== undefined) qs.set(k, v)
  const url = `/api${path}${qs.size ? `?${qs}` : ''}`
  let res: Response
  try {
    res = await fetch(url, { headers: { accept: 'application/json' } })
  } catch (e) {
    throw new ApiError(0, 'offline', `cannot reach agentvcd (${String(e)})`)
  }
  const body: unknown = await res.json().catch(() => null)
  if (!res.ok) {
    const err = (body as ApiErrorBody | null)?.error
    throw new ApiError(res.status, err?.code ?? 'http', err?.message ?? res.statusText)
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
}

export const ref = {
  attempt: (id: number): Ref => `a${id}`,
  snapshot: (id: number): Ref => `s${id}`,
}
