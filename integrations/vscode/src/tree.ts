// The Attempts view: tasks → attempts (nested by fork) → changes and snapshots.

import * as vscode from 'vscode'
import type { Connection } from './connection'
import { errorText } from './connection'
import { fileUri } from './content'
import {
  attemptDescription,
  attemptForest,
  countsLabel,
  isOpen,
  kindLetter,
  snapshotBase,
  snapshotLabel,
  type AttemptNode,
} from './model'
import type { Attempt, AttemptStats, FileChange, LiveAttempt, Ref, Snapshot, TaskSummary } from './types'

export type Node =
  | { kind: 'task'; task: TaskSummary }
  | { kind: 'loose' }
  | { kind: 'attempt'; node: AttemptNode }
  | { kind: 'changes'; attempt: Attempt }
  | { kind: 'change'; attempt: Attempt; change: FileChange; left: Ref; right: Ref }
  | { kind: 'snapshots'; attempt: Attempt }
  | { kind: 'snapshot'; attempt: Attempt; snapshot: Snapshot; all: Snapshot[] }
  | { kind: 'error'; message: string }

const STATUS_ICON: Record<string, [string, string | undefined]> = {
  exploring: ['circle-large-outline', 'charts.blue'],
  passed: ['pass', 'testing.iconPassed'],
  failed: ['error', 'testing.iconFailed'],
  promoted: ['star-full', 'charts.yellow'],
  abandoned: ['circle-slash', 'disabledForeground'],
}

function statusIcon(a: Attempt, running: boolean): vscode.ThemeIcon {
  if (running) return new vscode.ThemeIcon('sync~spin', new vscode.ThemeColor('charts.blue'))
  const [id, color] = STATUS_ICON[a.status] ?? ['circle-outline', undefined]
  return new vscode.ThemeIcon(id, color ? new vscode.ThemeColor(color) : undefined)
}

/** Ref shown for an attempt's own changes: the fork point (parent snapshot). */
function forkBase(a: Attempt): Ref | undefined {
  return a.parent_snapshot === null ? undefined : `s${a.parent_snapshot}`
}

export class AttemptsTree implements vscode.TreeDataProvider<Node> {
  private readonly _onDidChange = new vscode.EventEmitter<Node | undefined>()
  readonly onDidChangeTreeData = this._onDidChange.event

  private tasks: TaskSummary[] = []
  private attempts: Attempt[] = []
  private live = new Map<number, LiveAttempt>()
  private stats = new Map<number, AttemptStats>()
  private statsLoaded = new Set<number>()
  private loadError: string | undefined
  /** attempt id → its node, for reveal and getParent. */
  private nodes = new Map<number, Node & { kind: 'attempt' }>()
  private taskNodes = new Map<number, Node & { kind: 'task' }>()
  private looseNode: Node & { kind: 'loose' } = { kind: 'loose' }
  private parents = new WeakMap<object, Node | undefined>()
  private loading: Promise<void> | undefined
  private reloadQueued = false

  constructor(private readonly conn: Connection) {}

  /** All attempts from the last load. */
  get allAttempts(): readonly Attempt[] {
    return this.attempts
  }

  get allTasks(): readonly TaskSummary[] {
    return this.tasks
  }

  liveOf(id: number): LiveAttempt | undefined {
    return this.live.get(id)
  }

  /** Reloads from the backend and redraws. Concurrent calls coalesce. */
  async reload(): Promise<void> {
    if (this.loading) {
      this.reloadQueued = true
      return this.loading
    }
    this.loading = this.doReload().finally(() => {
      this.loading = undefined
      if (this.reloadQueued) {
        this.reloadQueued = false
        void this.reload()
      }
    })
    return this.loading
  }

  private async doReload() {
    if (this.conn.state !== 'connected' && this.conn.state !== 'cli') {
      this.tasks = []
      this.attempts = []
      this.live.clear()
      this.loadError = undefined
      this.rebuild()
      return
    }
    try {
      const b = this.conn.backend
      const [tasks, attempts, live] = await Promise.all([b.tasks(), b.attempts(), b.live().catch(() => [])])
      this.tasks = tasks.sort((x, y) => y.updated_at - x.updated_at)
      this.attempts = attempts
      this.live = new Map(live.map((l) => [l.attempt.id, l]))
      for (const l of live) this.stats.set(l.attempt.id, l.stats)
      // Stats of expanded tasks are refetched lazily on the next expand.
      this.statsLoaded.clear()
      this.loadError = undefined
      this.conn.setLiveRuns(live.some((l) => l.run?.alive))
    } catch (e) {
      this.loadError = errorText(e)
    }
    this.rebuild()
  }

  private rebuild() {
    this.nodes.clear()
    this.taskNodes.clear()
    this.parents = new WeakMap()
    this._onDidChange.fire(undefined)
  }

  private attemptNodes(forest: AttemptNode[], parent: Node | undefined): Node[] {
    return forest.map((n) => {
      const node = { kind: 'attempt' as const, node: n }
      this.nodes.set(n.attempt.id, node)
      this.parents.set(node, parent)
      return node
    })
  }

  getParent(el: Node): Node | undefined {
    return this.parents.get(el)
  }

  /** The tree node for an attempt, building the path down to it. */
  async nodeFor(id: number): Promise<Node | undefined> {
    if (!this.nodes.has(id)) {
      await this.getChildren()
      const a = this.attempts.find((x) => x.id === id)
      if (!a) return undefined
      const top = a.task_id === null ? this.looseNode : this.taskNodes.get(a.task_id)
      if (top) await this.expandTo(top, id)
    }
    return this.nodes.get(id)
  }

  private async expandTo(n: Node, id: number): Promise<boolean> {
    if (n.kind === 'attempt' && n.node.attempt.id === id) return true
    for (const c of await this.getChildren(n)) {
      if (c.kind !== 'attempt' && c.kind !== 'task') continue
      if (await this.expandTo(c, id)) return true
    }
    return false
  }

  async getChildren(el?: Node): Promise<Node[]> {
    if (!el) {
      if (this.loadError) return [{ kind: 'error', message: this.loadError }]
      const out: Node[] = this.tasks.map((task) => {
        const n = { kind: 'task' as const, task }
        this.taskNodes.set(task.id, n)
        return n
      })
      if (this.attempts.some((a) => a.task_id === null)) out.push(this.looseNode)
      return out
    }
    const b = this.conn.backend
    try {
      switch (el.kind) {
        case 'task': {
          await this.loadStats(el.task.id)
          const mine = this.attempts.filter((a) => a.task_id === el.task.id)
          return this.attemptNodes(attemptForest(mine), el)
        }
        case 'loose':
          return this.attemptNodes(
            attemptForest(this.attempts.filter((a) => a.task_id === null)),
            el,
          )
        case 'attempt': {
          const a = el.node.attempt
          const kids: Node[] = []
          if (forkBase(a)) kids.push(this.child(el, { kind: 'changes', attempt: a }))
          kids.push(this.child(el, { kind: 'snapshots', attempt: a }))
          return [...kids, ...this.attemptNodes(el.node.children, el)]
        }
        case 'changes': {
          const left = forkBase(el.attempt)!
          const right: Ref = `a${el.attempt.id}`
          const changes = await b.diff(right, left)
          return changes
            .sort((x, y) => x.path.localeCompare(y.path))
            .map((change) => this.child(el, { kind: 'change', attempt: el.attempt, change, left, right }))
        }
        case 'snapshots': {
          const all = await b.snapshots(el.attempt.id)
          return [...all]
            .reverse()
            .map((snapshot) => this.child(el, { kind: 'snapshot', attempt: el.attempt, snapshot, all }))
        }
        default:
          return []
      }
    } catch (e) {
      return [{ kind: 'error', message: errorText(e) }]
    }
  }

  private child<T extends Node>(parent: Node, n: T): T {
    this.parents.set(n, parent)
    return n
  }

  private async loadStats(task: number) {
    if (this.statsLoaded.has(task)) return
    this.statsLoaded.add(task)
    try {
      for (const s of await this.conn.backend.taskStats(task)) this.stats.set(s.attempt, s)
    } catch {
      /* stats are decoration only */
    }
  }

  getTreeItem(el: Node): vscode.TreeItem {
    switch (el.kind) {
      case 'task': {
        const t = el.task
        const item = new vscode.TreeItem(`t${t.id}  ${t.goal}`, vscode.TreeItemCollapsibleState.Expanded)
        item.id = `task:${t.id}`
        item.description = countsLabel(t.attempts)
        const done = t.attempts.promoted > 0
        const ready = !done && t.attempts.passed > 0
        item.iconPath = new vscode.ThemeIcon(
          done ? 'verified-filled' : ready ? 'inbox' : 'tasklist',
          done ? new vscode.ThemeColor('charts.yellow') : ready ? new vscode.ThemeColor('charts.green') : undefined,
        )
        item.contextValue = done ? 'task:done' : ready ? 'task:ready' : 'task'
        const md = new vscode.MarkdownString(`**t${t.id}** ${t.goal}`)
        if (t.criteria) md.appendMarkdown(`\n\n*Done when:* ${t.criteria}`)
        md.appendMarkdown(`\n\n${countsLabel(t.attempts)}`)
        if (ready) md.appendMarkdown('\n\n$(inbox) Ready to pick a winner')
        md.supportThemeIcons = true
        item.tooltip = md
        return item
      }
      case 'loose': {
        const item = new vscode.TreeItem('Attempts without a task', vscode.TreeItemCollapsibleState.Collapsed)
        item.id = 'loose'
        item.iconPath = new vscode.ThemeIcon('folder')
        return item
      }
      case 'attempt':
        return this.attemptItem(el.node)
      case 'changes': {
        const item = new vscode.TreeItem('Changes', vscode.TreeItemCollapsibleState.Collapsed)
        item.id = `changes:${el.attempt.id}`
        item.description = `vs s${el.attempt.parent_snapshot}`
        item.iconPath = new vscode.ThemeIcon('diff')
        item.contextValue = 'changes'
        item.command = {
          command: 'agentvc.compareWithParent',
          title: 'Show Changes',
          arguments: [{ kind: 'attempt', node: { attempt: el.attempt, children: [] } }],
        }
        return item
      }
      case 'change': {
        const { change, left, right } = el
        const uri = fileUri(right, change.path)
        const item = new vscode.TreeItem(uri, vscode.TreeItemCollapsibleState.None)
        item.id = `change:${el.attempt.id}:${change.path}`
        item.label = change.path.split('/').pop()
        const dir = change.path.includes('/') ? change.path.slice(0, change.path.lastIndexOf('/')) : ''
        item.description = `${kindLetter(change.kind)}${dir ? `  ${dir}` : ''}`
        item.tooltip = `${change.path} (${change.kind})`
        item.contextValue = 'change'
        item.command = {
          command: 'vscode.diff',
          title: 'Open Diff',
          arguments: [
            fileUri(left, change.path),
            uri,
            `${change.path} (${left} ↔ ${right})`,
            { preview: true },
          ],
        }
        return item
      }
      case 'snapshots': {
        const item = new vscode.TreeItem('Snapshots', vscode.TreeItemCollapsibleState.Collapsed)
        item.id = `snaps:${el.attempt.id}`
        const s = this.stats.get(el.attempt.id)
        if (s) item.description = String(s.snapshots)
        item.iconPath = new vscode.ThemeIcon('history')
        return item
      }
      case 'snapshot': {
        const s = el.snapshot
        const item = new vscode.TreeItem(snapshotLabel(s), vscode.TreeItemCollapsibleState.None)
        item.id = `snap:${s.id}`
        item.description = new Date(s.created_at * 1000).toLocaleString()
        item.iconPath = new vscode.ThemeIcon('git-commit')
        item.contextValue = 'snapshot'
        const md = new vscode.MarkdownString(`**s${s.id}** · ${s.trigger}\n\ntree \`${s.tree.slice(0, 12)}\``)
        if (s.meta) md.appendCodeblock(JSON.stringify(s.meta, null, 2), 'json')
        item.tooltip = md
        const base = snapshotBase(s, el.all, el.attempt)
        if (base) {
          item.command = {
            command: 'agentvc.openSnapshotChanges',
            title: 'Open Snapshot Changes',
            arguments: [el],
          }
        }
        return item
      }
      case 'error': {
        const item = new vscode.TreeItem(el.message)
        item.iconPath = new vscode.ThemeIcon('warning', new vscode.ThemeColor('problemsWarningIcon.foreground'))
        return item
      }
    }
  }

  private attemptItem(n: AttemptNode): vscode.TreeItem {
    const a = n.attempt
    const live = this.live.get(a.id)
    const running = !!live?.run?.alive
    const stats = this.stats.get(a.id)
    const item = new vscode.TreeItem(`a${a.id}  ${a.actor.replace(/^agent:/, '')}`, vscode.TreeItemCollapsibleState.Collapsed)
    item.id = `attempt:${a.id}`
    item.description = attemptDescription(a, stats, live)
    item.iconPath = statusIcon(a, running)
    item.contextValue = `attempt:${isOpen(a.status) ? 'open' : 'closed'}${running ? ':running' : ''}`
    const md = new vscode.MarkdownString(undefined, true)
    md.appendMarkdown(`**a${a.id}** · ${a.status}${a.reason ? ` (${a.reason})` : ''}\n\n`)
    md.appendMarkdown(`actor \`${a.actor}\`${a.model ? ` · model \`${a.model}\`` : ''}\n\n`)
    if (a.parent_attempt !== null) md.appendMarkdown(`forked from a${a.parent_attempt} at s${a.parent_snapshot}\n\n`)
    if (stats?.check) md.appendMarkdown(`check: ${stats.check.passed ? '$(pass) passed' : '$(error) failed'} (exit ${stats.check.exit_code ?? '?'})\n\n`)
    if (stats?.summary) md.appendMarkdown(`${stats.summary}\n\n`)
    if (a.prompt) md.appendMarkdown(`> ${a.prompt.split('\n').join('\n> ')}\n\n`)
    if (running && live?.run) {
      md.appendMarkdown(`$(sync~spin) running \`${live.run.command.join(' ')}\`\n`)
      if (live.run.log_tail.length) md.appendCodeblock(live.run.log_tail.slice(-8).join('\n'), 'log')
    }
    md.appendMarkdown(`\nview \`${a.view_path}\``)
    item.tooltip = md
    return item
  }
}
