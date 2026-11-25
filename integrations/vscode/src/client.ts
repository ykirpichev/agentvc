// Talks to agentvc: over agentvcd's HTTP API when a daemon is reachable,
// else through the `agentvc` CLI with `--json`. Free of `vscode` imports so
// it can be unit-tested in plain Node.

import { execFile } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import * as path from 'node:path'
import type {
  ActionResponse,
  Attempt,
  AttemptDetail,
  AttemptStats,
  DiffResponse,
  FileChange,
  FileResponse,
  LiveAttempt,
  Ref,
  Snapshot,
  SpaceInfo,
  StatusCounts,
  StopResponse,
  Task,
  TaskSummary,
} from './types'

/** An error carrying agentvc's stable error `code`. */
export class AgentvcError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly status = 0,
  ) {
    super(message)
    this.name = 'AgentvcError'
  }
}

export type MarkStatus = 'passed' | 'failed' | 'exploring'

/** Everything the extension reads or changes. */
export interface Backend {
  readonly kind: 'daemon' | 'cli'
  space(): Promise<SpaceInfo>
  tasks(): Promise<TaskSummary[]>
  attempts(task?: number): Promise<Attempt[]>
  attempt(id: number): Promise<AttemptDetail>
  snapshots(attempt: number): Promise<Snapshot[]>
  /** Without `a`, diffs against the snapshot `b` was forked from. */
  diff(b: Ref, a?: Ref): Promise<FileChange[]>
  /** File content at a ref, or null when the file does not exist there. */
  file(ref: Ref, path: string): Promise<FileResponse | null>
  taskStats(task: number): Promise<AttemptStats[]>
  live(): Promise<LiveAttempt[]>
  promote(id: number): Promise<ActionResponse>
  abandon(id: number, reason: string): Promise<ActionResponse>
  mark(id: number, status: MarkStatus): Promise<ActionResponse>
  stop(id: number): Promise<StopResponse>
}

type FetchLike = typeof fetch

/** agentvcd's JSON API. */
export class DaemonBackend implements Backend {
  readonly kind = 'daemon' as const
  readonly baseUrl: string

  constructor(
    baseUrl: string,
    private readonly fetchImpl: FetchLike = fetch,
    private readonly timeoutMs = 10_000,
  ) {
    this.baseUrl = baseUrl.replace(/\/+$/, '')
  }

  space = () => this.get<SpaceInfo>('/space')
  tasks = () => this.get<TaskSummary[]>('/tasks')
  attempts = (task?: number) =>
    this.get<Attempt[]>('/attempts', { task: task === undefined ? undefined : String(task) })
  attempt = (id: number) => this.get<AttemptDetail>(`/attempts/${id}`)
  snapshots = (attempt: number) => this.get<Snapshot[]>(`/attempts/${attempt}/snapshots`)
  diff = async (b: Ref, a?: Ref) => (await this.get<DiffResponse>('/diff', { a, b })).changes
  file = async (ref: Ref, p: string) => {
    try {
      return await this.get<FileResponse>('/file', { ref, path: p })
    } catch (e) {
      if (e instanceof AgentvcError && e.code === 'not_found' && e.message.includes(p)) return null
      throw e
    }
  }
  taskStats = (task: number) => this.get<AttemptStats[]>(`/tasks/${task}/stats`)
  live = () => this.get<LiveAttempt[]>('/live')
  promote = (id: number) => this.post<ActionResponse>(`/attempts/${id}/promote`)
  abandon = (id: number, reason: string) =>
    this.post<ActionResponse>(`/attempts/${id}/abandon`, { reason })
  mark = (id: number, status: MarkStatus) =>
    this.post<ActionResponse>(`/attempts/${id}/mark`, { status })
  stop = (id: number) => this.post<StopResponse>(`/attempts/${id}/stop`)

  /** WebSocket URL of the live event stream. */
  get eventsUrl(): string {
    return `${this.baseUrl.replace(/^http/, 'ws')}/api/events`
  }

  private get<T>(p: string, params?: Record<string, string | undefined>): Promise<T> {
    const qs = new URLSearchParams()
    for (const [k, v] of Object.entries(params ?? {})) if (v !== undefined) qs.set(k, v)
    const q = qs.toString()
    return this.request<T>(`/api${p}${q ? `?${q}` : ''}`)
  }

  private post<T>(p: string, body?: unknown): Promise<T> {
    return this.request<T>(`/api${p}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  }

  private async request<T>(p: string, init: RequestInit = {}): Promise<T> {
    let res: Response
    try {
      res = await this.fetchImpl(this.baseUrl + p, {
        ...init,
        headers: { accept: 'application/json', ...(init.headers as Record<string, string>) },
        signal: AbortSignal.timeout(this.timeoutMs),
      })
    } catch (e) {
      throw new AgentvcError('offline', `cannot reach agentvcd at ${this.baseUrl} (${String(e)})`)
    }
    const body: unknown = await res.json().catch(() => null)
    if (!res.ok) {
      const err = (body as { error?: { code?: string; message?: string } } | null)?.error
      throw new AgentvcError(err?.code ?? 'http', err?.message ?? res.statusText, res.status)
    }
    return body as T
  }
}

/** Runs `agentvc --json` and returns the parsed object, or throws its error. */
export type CliRunner = (args: string[]) => Promise<Record<string, unknown>>

export function cliRunner(cliPath: string, cwd: string, timeoutMs = 120_000): CliRunner {
  return (args) =>
    new Promise((resolve, reject) => {
      execFile(
        cliPath,
        ['--json', ...args],
        { cwd, timeout: timeoutMs, maxBuffer: 64 * 1024 * 1024 },
        (err, stdout, stderr) => {
          const line = stdout.trim().split('\n').pop() ?? ''
          let out: Record<string, unknown> | null = null
          try {
            out = JSON.parse(line) as Record<string, unknown>
          } catch {
            /* fall through */
          }
          if (out && out.ok === false) {
            const e = out.error as { code?: string; message?: string } | undefined
            reject(new AgentvcError(e?.code ?? 'cli', e?.message ?? 'agentvc failed'))
          } else if (out) {
            resolve(out)
          } else if (err && (err as NodeJS.ErrnoException).code === 'ENOENT') {
            reject(
              new AgentvcError(
                'cli_missing',
                `cannot run ${cliPath}: install agentvc or set agentvc.cliPath`,
              ),
            )
          } else {
            reject(new AgentvcError('cli', (stderr || String(err ?? 'no output')).trim()))
          }
        },
      )
    })
}

const ZERO: StatusCounts = { total: 0, exploring: 0, passed: 0, failed: 0, abandoned: 0, promoted: 0 }

export function countStatuses(attempts: Attempt[]): StatusCounts {
  const c = { ...ZERO }
  for (const a of attempts) {
    c.total++
    c[a.status]++
  }
  return c
}

/**
 * The CLI fallback, used when no daemon is reachable. It covers the tree and
 * the review actions; file content at a snapshot needs the daemon, so diffs
 * read an attempt's current view instead.
 */
export class CliBackend implements Backend {
  readonly kind = 'cli' as const

  constructor(private readonly run: CliRunner) {}

  async space(): Promise<SpaceInfo> {
    const all = await this.attempts()
    const first = all.find((a) => a.parent_attempt === null)
    const workdir = first ? path.resolve(first.view_path) : ''
    return { workdir, name: path.basename(workdir), format_version: 0, daemon_version: 'cli' }
  }

  async tasks(): Promise<TaskSummary[]> {
    const [{ tasks }, attempts] = await Promise.all([
      this.run(['task', 'list']) as Promise<{ tasks: Task[] }>,
      this.attempts(),
    ])
    return tasks.map((t) => {
      const mine = attempts.filter((a) => a.task_id === t.id)
      return {
        ...t,
        attempts: countStatuses(mine),
        updated_at: Math.max(t.created_at, ...mine.map((a) => a.created_at)),
      }
    })
  }

  async attempts(task?: number): Promise<Attempt[]> {
    const out = (await this.run(task === undefined ? ['log'] : ['log', '--task', `t${task}`])) as {
      attempts: Attempt[]
    }
    return out.attempts
  }

  async attempt(id: number): Promise<AttemptDetail> {
    const out = (await this.run(['show', `a${id}`])) as { attempt: Attempt; snapshots: Snapshot[] }
    const all = await this.attempts()
    return {
      attempt: out.attempt,
      head: out.snapshots.at(-1) ?? null,
      snapshot_count: out.snapshots.length,
      children: all.filter((a) => a.parent_attempt === id).map((a) => a.id),
      // Rebase info is served by the daemon only.
      git_base: null,
      base_commit: null,
    }
  }

  async snapshots(attempt: number): Promise<Snapshot[]> {
    return ((await this.run(['show', `a${attempt}`])) as { snapshots: Snapshot[] }).snapshots
  }

  async diff(b: Ref, a?: Ref): Promise<FileChange[]> {
    const args = a === undefined ? ['diff', b] : ['diff', a, b]
    return ((await this.run(args)) as { changes: FileChange[] }).changes
  }

  async file(ref: Ref, p: string): Promise<FileResponse | null> {
    if (!ref.startsWith('a')) {
      throw new AgentvcError(
        'needs_daemon',
        'reading a snapshot needs agentvcd; start it with "agentvc: Start agentvcd"',
      )
    }
    const { path: view } = (await this.run(['path', ref])) as { path: string }
    let buf: Buffer
    try {
      buf = await readFile(path.join(view, p))
    } catch {
      return null
    }
    const binary = buf.subarray(0, 8000).includes(0)
    return {
      ref,
      snapshot: 0,
      path: p,
      mode: 'file',
      oid: '',
      size: buf.length,
      binary,
      too_large: false,
      content: binary ? null : buf.toString('utf8'),
    }
  }

  async taskStats(): Promise<AttemptStats[]> {
    return []
  }

  async live(): Promise<LiveAttempt[]> {
    return []
  }

  async promote(id: number): Promise<ActionResponse> {
    const out = (await this.run(['promote', `a${id}`])) as {
      attempt: Attempt
      closed_siblings: number[]
    }
    return { attempt: out.attempt, closed_siblings: out.closed_siblings ?? [] }
  }

  async abandon(id: number, reason: string): Promise<ActionResponse> {
    const out = (await this.run(['abandon', `a${id}`, '--reason', reason])) as { attempt: Attempt }
    return { attempt: out.attempt, closed_siblings: [] }
  }

  async mark(id: number, status: MarkStatus): Promise<ActionResponse> {
    const out = (await this.run(['mark', `a${id}`, status])) as { attempt: Attempt }
    return { attempt: out.attempt, closed_siblings: [] }
  }

  async stop(): Promise<StopResponse> {
    throw new AgentvcError('needs_daemon', 'stopping a run needs agentvcd')
  }
}
