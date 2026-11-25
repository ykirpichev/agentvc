// Finds the space for the workspace and connects to it: an already running
// agentvcd, one we start ourselves, or the CLI as a fallback. Turns the
// daemon's WebSocket events into one debounced "changed" signal.

import { spawn, type ChildProcess } from 'node:child_process'
import { existsSync, statSync } from 'node:fs'
import * as path from 'node:path'
import * as vscode from 'vscode'
import WebSocket from 'ws'
import { AgentvcError, CliBackend, DaemonBackend, cliRunner, type Backend } from './client'
import type { DaemonEvent } from './types'

export type ConnState = 'connecting' | 'connected' | 'cli' | 'offline' | 'nospace'

/** The directory holding `.agentvc` at or above `dir`. */
export function findSpaceRoot(dir: string): string | undefined {
  let d = path.resolve(dir)
  for (;;) {
    const marker = path.join(d, '.agentvc')
    try {
      if (existsSync(marker) && statSync(marker).isDirectory()) return d
    } catch {
      /* keep walking */
    }
    const up = path.dirname(d)
    if (up === d) return undefined
    d = up
  }
}

function config() {
  const c = vscode.workspace.getConfiguration('agentvc')
  return {
    daemonUrl: c.get<string>('daemonUrl', 'http://127.0.0.1:7420'),
    autoStart: c.get<boolean>('autoStartDaemon', true),
    cliPath: c.get<string>('cliPath', 'agentvc') || 'agentvc',
    daemonPath: c.get<string>('daemonPath', 'agentvcd') || 'agentvcd',
  }
}

/** Minimum gap between refreshes triggered by events; live runs poll no faster than this. */
const DEBOUNCE_MS = 300
const LIVE_POLL_MS = 3000

export class Connection implements vscode.Disposable {
  private _state: ConnState = 'connecting'
  private _backend: Backend | undefined
  private _root: string | undefined
  private child: ChildProcess | undefined
  private ws: WebSocket | undefined
  private wsRetry: NodeJS.Timeout | undefined
  private wsBackoff = 1000
  private debounce: NodeJS.Timeout | undefined
  private livePoll: NodeJS.Timeout | undefined
  private disposed = false
  private readonly _onDidChange = new vscode.EventEmitter<DaemonEvent | undefined>()
  private readonly _onDidChangeState = new vscode.EventEmitter<ConnState>()
  /** Fires (debounced) when the space changed, with the last event if any. */
  readonly onDidChange = this._onDidChange.event
  readonly onDidChangeState = this._onDidChangeState.event

  constructor(private readonly log: vscode.OutputChannel) {}

  get state(): ConnState {
    return this._state
  }

  /** The space's work directory, once found. */
  get root(): string | undefined {
    return this._root
  }

  get backend(): Backend {
    if (!this._backend) throw new AgentvcError('offline', 'not connected to agentvc')
    return this._backend
  }

  get daemon(): DaemonBackend | undefined {
    return this._backend instanceof DaemonBackend ? this._backend : undefined
  }

  /** Runs the CLI in the space, for commands the daemon does not serve (fork, pr). */
  cli(args: string[]): Promise<Record<string, unknown>> {
    const root = this._root ?? this.workspaceDir()
    if (!root) return Promise.reject(new AgentvcError('no_space', 'open a folder with an agentvc space'))
    return cliRunner(config().cliPath, root, 10 * 60_000)(args)
  }

  private workspaceDir(): string | undefined {
    return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath
  }

  private setState(s: ConnState) {
    this._state = s
    void vscode.commands.executeCommand('setContext', 'agentvc.state', s)
    this._onDidChangeState.fire(s)
  }

  /** (Re)connects: configured daemon, else our own daemon, else the CLI. */
  async connect(opts: { forceStart?: boolean } = {}): Promise<ConnState> {
    this.closeEvents()
    this.setState('connecting')
    const folders = vscode.workspace.workspaceFolders ?? []
    this._root = undefined
    for (const f of folders) {
      this._root = findSpaceRoot(f.uri.fsPath)
      if (this._root) break
    }
    const cfg = config()

    // 1. A daemon that is already running for this space.
    const configured = new DaemonBackend(cfg.daemonUrl, fetch, 2000)
    try {
      const info = await configured.space()
      if (!this._root || samePath(info.workdir, this._root)) {
        this._root = info.workdir
        return this.useDaemon(new DaemonBackend(cfg.daemonUrl))
      }
      this.log.appendLine(`agentvcd at ${cfg.daemonUrl} serves ${info.workdir}, not ${this._root}`)
    } catch (e) {
      this.log.appendLine(`no daemon at ${cfg.daemonUrl}: ${String(e)}`)
    }

    if (!this._root) {
      this.setState('nospace')
      return this._state
    }

    // 2. One we start (or already started) for this workspace.
    if (this.child && this.childUrl) {
      try {
        const d = new DaemonBackend(this.childUrl)
        await d.space()
        return this.useDaemon(d)
      } catch {
        this.stopChild()
      }
    }
    if (cfg.autoStart || opts.forceStart) {
      try {
        const url = await this.startDaemon(cfg.daemonPath, this._root)
        return this.useDaemon(new DaemonBackend(url))
      } catch (e) {
        this.log.appendLine(`could not start agentvcd: ${String(e)}`)
        if (opts.forceStart) void vscode.window.showErrorMessage(`agentvc: ${errorText(e)}`)
      }
    }

    // 3. The CLI.
    const cli = new CliBackend(cliRunner(cfg.cliPath, this._root))
    try {
      await cli.attempts()
      this._backend = cli
      this.setState('cli')
      this._onDidChange.fire(undefined)
    } catch (e) {
      this.log.appendLine(`CLI fallback failed: ${String(e)}`)
      this._backend = undefined
      this.setState(e instanceof AgentvcError && e.code === 'no_space' ? 'nospace' : 'offline')
    }
    return this._state
  }

  private useDaemon(d: DaemonBackend): ConnState {
    this._backend = d
    this.log.appendLine(`connected to agentvcd at ${d.baseUrl} for ${this._root}`)
    this.setState('connected')
    this.openEvents(d)
    this._onDidChange.fire(undefined)
    return this._state
  }

  private childUrl: string | undefined

  private startDaemon(bin: string, root: string): Promise<string> {
    this.stopChild()
    return new Promise((resolve, reject) => {
      const child = spawn(bin, ['--json', '--port', '0', '-C', root], {
        cwd: root,
        stdio: ['ignore', 'pipe', 'pipe'],
      })
      this.child = child
      let buf = ''
      const timer = setTimeout(() => reject(new Error('agentvcd did not start within 10s')), 10_000)
      child.on('error', (e) => {
        clearTimeout(timer)
        this.child = undefined
        reject(e)
      })
      child.stderr?.on('data', (d: Buffer) => this.log.append(`[agentvcd] ${d.toString()}`))
      child.stdout?.on('data', (d: Buffer) => {
        buf += d.toString()
        const nl = buf.indexOf('\n')
        if (nl < 0 || this.childUrl) return
        clearTimeout(timer)
        try {
          const out = JSON.parse(buf.slice(0, nl)) as {
            ok: boolean
            url?: string
            error?: { message: string }
          }
          if (out.ok && out.url) {
            this.childUrl = out.url
            this.log.appendLine(`started agentvcd at ${out.url} (pid ${child.pid})`)
            resolve(out.url)
          } else reject(new Error(out.error?.message ?? 'agentvcd failed to start'))
        } catch (e) {
          reject(e as Error)
        }
      })
      child.on('exit', (code) => {
        this.log.appendLine(`agentvcd exited (${code})`)
        if (this.child === child) {
          this.child = undefined
          this.childUrl = undefined
          if (!this.disposed && this._state === 'connected') void this.connect()
        }
      })
    })
  }

  private stopChild() {
    const c = this.child
    this.child = undefined
    this.childUrl = undefined
    c?.kill()
  }

  /** Signals a change now (after a local action) instead of waiting for the event. */
  poke(): void {
    this.schedule(undefined)
  }

  private schedule(ev: DaemonEvent | undefined) {
    if (this.debounce) clearTimeout(this.debounce)
    this.debounce = setTimeout(() => {
      this.debounce = undefined
      this._onDidChange.fire(ev)
    }, DEBOUNCE_MS)
  }

  /** Keep polling the live board while runs are alive, so spinners and logs move. */
  setLiveRuns(alive: boolean): void {
    if (alive && !this.livePoll && this._state === 'connected') {
      this.livePoll = setInterval(() => this._onDidChange.fire(undefined), LIVE_POLL_MS)
    } else if (!alive && this.livePoll) {
      clearInterval(this.livePoll)
      this.livePoll = undefined
    }
  }

  private openEvents(d: DaemonBackend) {
    this.closeEvents()
    const ws = new WebSocket(d.eventsUrl)
    this.ws = ws
    ws.on('open', () => {
      this.wsBackoff = 1000
    })
    ws.on('message', (data) => {
      let ev: DaemonEvent | undefined
      try {
        ev = JSON.parse(data.toString()) as DaemonEvent
      } catch {
        return
      }
      if (ev.type === 'hello') return
      this.schedule(ev)
    })
    ws.on('error', (e) => this.log.appendLine(`events: ${String(e)}`))
    ws.on('close', () => {
      if (this.ws !== ws || this.disposed) return
      this.ws = undefined
      this.wsRetry = setTimeout(async () => {
        try {
          await d.space()
          this.openEvents(d)
          this.schedule({ type: 'resync' })
        } catch {
          this.wsBackoff = Math.min(this.wsBackoff * 2, 30_000)
          void this.connect()
        }
      }, this.wsBackoff)
    })
  }

  private closeEvents() {
    if (this.wsRetry) clearTimeout(this.wsRetry)
    this.wsRetry = undefined
    const ws = this.ws
    this.ws = undefined
    ws?.removeAllListeners('close')
    ws?.on('error', () => undefined)
    ws?.close()
  }

  dispose(): void {
    this.disposed = true
    this.closeEvents()
    this.setLiveRuns(false)
    if (this.debounce) clearTimeout(this.debounce)
    this.stopChild()
    this._onDidChange.dispose()
    this._onDidChangeState.dispose()
  }
}

function samePath(a: string, b: string): boolean {
  return path.resolve(a) === path.resolve(b)
}

export function errorText(e: unknown): string {
  if (e instanceof AgentvcError) return `${e.message} [${e.code}]`
  return e instanceof Error ? e.message : String(e)
}
