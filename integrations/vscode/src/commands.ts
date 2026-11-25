// Commands: compare in the native diff editor, and the review actions.

import * as vscode from 'vscode'
import type { Connection } from './connection'
import { errorText } from './connection'
import { fileUri } from './content'
import { diffEntries, isOpen, kindLetter, snapshotBase, type DiffEntry } from './model'
import type { AttemptsTree, Node } from './tree'
import type { ActionResponse, Attempt, Ref } from './types'

export interface CompareResult {
  left: Ref
  right: Ref
  entries: DiffEntry[]
  /** [label, left, right] as passed to the diff editor. */
  resources: [vscode.Uri, vscode.Uri, vscode.Uri][]
}

export class Commands {
  private selected: Ref | undefined

  constructor(
    private readonly conn: Connection,
    private readonly tree: AttemptsTree,
    private readonly view: vscode.TreeView<Node>,
  ) {}

  register(ctx: vscode.ExtensionContext): void {
    const reg = (id: string, fn: (...args: never[]) => unknown) =>
      ctx.subscriptions.push(
        vscode.commands.registerCommand(id, async (...args: never[]) => {
          try {
            return await fn(...args)
          } catch (e) {
            void vscode.window.showErrorMessage(`agentvc: ${errorText(e)}`)
            return undefined
          }
        }),
      )
    reg('agentvc.refresh', () => this.refresh())
    reg('agentvc.startDaemon', () => this.conn.connect({ forceStart: true }))
    reg('agentvc.compareAttempts', (n?: Node, b?: Ref) => this.compareAttempts(n, b))
    reg('agentvc.compareWithParent', (n?: Node) => this.compareWithParent(n))
    reg('agentvc.selectForCompare', (n: Node) => this.selectForCompare(n))
    reg('agentvc.compareWithSelected', (n: Node) => this.compareWithSelected(n))
    reg('agentvc.openSnapshotChanges', (n: Node) => this.openSnapshotChanges(n))
    reg('agentvc.openFileDiff', (n?: Node) => this.openFileDiff(n))
    reg('agentvc.openView', (n?: Node) => this.openView(n, true))
    reg('agentvc.addViewToWorkspace', (n?: Node) => this.openView(n, false))
    reg('agentvc.fork', (n?: Node, opts?: { prompt?: string; quiet?: boolean }) => this.fork(n, opts))
    reg('agentvc.markPassed', (n?: Node) => this.mark(n, 'passed'))
    reg('agentvc.markFailed', (n?: Node) => this.mark(n, 'failed'))
    reg('agentvc.markExploring', (n?: Node) => this.mark(n, 'exploring'))
    reg('agentvc.promote', (n?: Node, opts?: { confirm?: boolean }) => this.promote(n, opts))
    reg('agentvc.abandon', (n?: Node, reason?: string) => this.abandon(n, reason))
    reg('agentvc.stopRun', (n?: Node) => this.stopRun(n))
    reg('agentvc.openPr', (n?: Node) => this.openPr(n))
    reg('agentvc.openInWebUi', (n?: Node) => this.openInWebUi(n))
    reg('agentvc.copyRef', (n: Node) => this.copyRef(n))
    reg('agentvc.revealCurrent', (id?: number) => this.reveal(id))
  }

  async refresh(): Promise<void> {
    if (this.conn.state === 'connected' || this.conn.state === 'cli') await this.tree.reload()
    else await this.conn.connect()
  }

  // ---- compare ------------------------------------------------------------

  /** Opens every change between two refs in the multi-file diff editor. */
  async openCompare(left: Ref, right: Ref, title?: string): Promise<CompareResult> {
    const changes = await this.conn.backend.diff(right, left)
    const entries = diffEntries(changes)
    const resources = entries.map(
      (e) =>
        [fileUri(e.right ? right : left, e.path), fileUri(left, e.path), fileUri(right, e.path)] as [
          vscode.Uri,
          vscode.Uri,
          vscode.Uri,
        ],
    )
    const result = { left, right, entries, resources }
    if (!entries.length) {
      void vscode.window.showInformationMessage(`agentvc: no differences between ${left} and ${right}`)
      return result
    }
    const label = title ?? `${left} ↔ ${right}`
    const commands = await vscode.commands.getCommands(true)
    if (commands.includes('vscode.changes')) {
      await vscode.commands.executeCommand('vscode.changes', label, resources)
    } else if (entries.length === 1) {
      await this.openOne(left, right, entries[0])
    } else {
      await this.pickAndOpen(left, right, entries)
    }
    return result
  }

  private async openOne(left: Ref, right: Ref, e: DiffEntry) {
    await vscode.commands.executeCommand(
      'vscode.diff',
      fileUri(left, e.path),
      fileUri(right, e.path),
      `${e.path} (${left} ↔ ${right})`,
    )
  }

  private async pickAndOpen(left: Ref, right: Ref, entries: DiffEntry[]) {
    const pick = await vscode.window.showQuickPick(
      entries.map((e) => ({ label: e.path, description: kindLetter(e.kind), entry: e })),
      { title: `${left} ↔ ${right}`, placeHolder: 'Changed file to open', matchOnDescription: true },
    )
    if (pick) await this.openOne(left, right, pick.entry)
  }

  async compareAttempts(n?: Node, other?: Ref): Promise<CompareResult | undefined> {
    if (n?.kind === 'attempt' && other) return this.openCompare(other, `a${n.node.attempt.id}`)
    const task = n?.kind === 'task' ? n.task.id : undefined
    const pool = this.tree.allAttempts.filter((a) => task === undefined || a.task_id === task)
    if (pool.length < 2) {
      void vscode.window.showInformationMessage('agentvc: need two attempts to compare')
      return undefined
    }
    const items = [...pool]
      .sort((x, y) => y.id - x.id)
      .map((a) => ({
        label: `a${a.id}`,
        description: `${a.status} · ${a.actor}${a.task_id === null ? '' : ` · t${a.task_id}`}`,
        detail: a.prompt?.split('\n')[0],
        picked: false,
        attempt: a,
      }))
    // Preselect the two newest passed attempts of one task: the usual "which one wins?" question.
    const byTask = new Map<number, typeof items>()
    for (const i of items) {
      if (i.attempt.status !== 'passed' || i.attempt.task_id === null) continue
      byTask.set(i.attempt.task_id, [...(byTask.get(i.attempt.task_id) ?? []), i])
    }
    const pair = [...byTask.values()].find((xs) => xs.length >= 2)
    if (pair) {
      pair[0].picked = true
      pair[1].picked = true
    }
    const picks = await vscode.window.showQuickPick(items, {
      title: task === undefined ? 'Compare two attempts' : `Compare attempts of t${task}`,
      placeHolder: 'Pick two attempts',
      canPickMany: true,
    })
    if (!picks) return undefined
    if (picks.length !== 2) {
      void vscode.window.showWarningMessage('agentvc: pick exactly two attempts')
      return undefined
    }
    const [x, y] = picks.map((p) => p.attempt).sort((p, q) => p.id - q.id)
    return this.openCompare(`a${x.id}`, `a${y.id}`)
  }

  async compareWithParent(n?: Node): Promise<CompareResult | undefined> {
    const a = await this.attemptArg(n)
    if (!a) return undefined
    if (a.parent_snapshot === null) {
      void vscode.window.showInformationMessage(`agentvc: a${a.id} has no parent to compare with`)
      return undefined
    }
    return this.openCompare(`s${a.parent_snapshot}`, `a${a.id}`, `a${a.id} vs s${a.parent_snapshot}`)
  }

  private refOf(n: Node): Ref | undefined {
    if (n.kind === 'attempt') return `a${n.node.attempt.id}`
    if (n.kind === 'snapshot') return `s${n.snapshot.id}`
    return undefined
  }

  private selectForCompare(n: Node) {
    this.selected = this.refOf(n)
    void vscode.commands.executeCommand('setContext', 'agentvc.hasCompareSelection', !!this.selected)
  }

  private async compareWithSelected(n: Node) {
    const right = this.refOf(n)
    if (!this.selected || !right) return
    await this.openCompare(this.selected, right)
  }

  async openSnapshotChanges(n: Node): Promise<CompareResult | undefined> {
    if (n.kind !== 'snapshot') return undefined
    const base = snapshotBase(n.snapshot, n.all, n.attempt)
    if (!base) return undefined
    return this.openCompare(base, `s${n.snapshot.id}`, `s${n.snapshot.id} ${n.snapshot.trigger} (vs ${base})`)
  }

  private async openFileDiff(n?: Node) {
    const a = await this.attemptArg(n)
    if (!a || a.parent_snapshot === null) return
    const left: Ref = `s${a.parent_snapshot}`
    const right: Ref = `a${a.id}`
    const entries = diffEntries(await this.conn.backend.diff(right, left))
    if (!entries.length) {
      void vscode.window.showInformationMessage(`agentvc: a${a.id} has no changes`)
      return
    }
    await this.pickAndOpen(left, right, entries)
  }

  // ---- views and actions ----------------------------------------------------

  private async openView(n: Node | undefined, newWindow: boolean) {
    const a = await this.attemptArg(n)
    if (!a) return
    // `agentvc path` mounts an overlay view again if needed (after a reboot).
    let dir = a.view_path
    try {
      dir = ((await this.conn.cli(['path', `a${a.id}`])) as { path: string }).path
    } catch {
      /* use the recorded path */
    }
    const uri = vscode.Uri.file(dir)
    if (newWindow) {
      await vscode.commands.executeCommand('vscode.openFolder', uri, { forceNewWindow: true })
    } else {
      const at = vscode.workspace.workspaceFolders?.length ?? 0
      vscode.workspace.updateWorkspaceFolders(at, 0, { uri, name: `a${a.id} (${a.actor})` })
    }
  }

  async fork(n?: Node, opts: { prompt?: string; quiet?: boolean } = {}): Promise<Attempt | undefined> {
    const a = await this.attemptArg(n)
    if (!a) return undefined
    const prompt =
      opts.prompt ??
      (await vscode.window.showInputBox({
        title: `Fork a${a.id}`,
        prompt: 'What the new attempt should try (optional, recorded as its prompt)',
        ignoreFocusOut: true,
      }))
    if (prompt === undefined) return undefined
    const actor = vscode.workspace.getConfiguration('agentvc').get<string>('forkActor', 'human')
    const args = ['fork', '--from', `a${a.id}`, '--actor', actor]
    if (a.task_id !== null) args.push('--task', `t${a.task_id}`)
    if (prompt) args.push('--prompt', prompt)
    const out = (await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: `Forking a${a.id}…` },
      () => this.conn.cli(args),
    )) as { fork: { attempt: Attempt } }
    const created = out.fork.attempt
    this.conn.poke()
    if (!opts.quiet) {
      const choice = await vscode.window.showInformationMessage(
        `agentvc: forked a${created.id} from a${a.id}`,
        'Open in New Window',
        'Add to Workspace',
      )
      const node = { kind: 'attempt' as const, node: { attempt: created, children: [] } }
      if (choice === 'Open in New Window') await this.openView(node, true)
      else if (choice === 'Add to Workspace') await this.openView(node, false)
    }
    return created
  }

  private async mark(n: Node | undefined, status: 'passed' | 'failed' | 'exploring') {
    const a = await this.attemptArg(n, (x) => isOpen(x.status))
    if (!a) return undefined
    const r = await this.conn.backend.mark(a.id, status)
    this.done(r, `a${a.id} marked ${status}`)
    return r
  }

  /** Picks the winner, exactly like `agentvc promote aN`. */
  async promote(n?: Node, opts: { confirm?: boolean } = {}): Promise<ActionResponse | undefined> {
    const a = await this.attemptArg(n, (x) => isOpen(x.status))
    if (!a) return undefined
    // Mirrors core promotion: attempts the winner was forked from are promoted
    // with it ("continued in aN"); the task's other open attempts are abandoned.
    const byId = new Map(this.tree.allAttempts.map((x) => [x.id, x]))
    const ancestors = new Set<number>()
    for (let p = a.parent_attempt; p !== null && !ancestors.has(p); p = byId.get(p)?.parent_attempt ?? null) ancestors.add(p)
    const open = this.tree.allAttempts.filter(
      (x) => a.task_id !== null && x.task_id === a.task_id && x.id !== a.id && isOpen(x.status),
    )
    const losers = open.filter((x) => !ancestors.has(x.id))
    const carried = open.filter((x) => ancestors.has(x.id))
    const ask =
      opts.confirm ?? vscode.workspace.getConfiguration('agentvc').get<boolean>('confirmPromote', true)
    if (ask) {
      const refs = (xs: Attempt[]) => xs.map((x) => `a${x.id}`).join(', ')
      const lines = [
        losers.length ? `${refs(losers)} will be abandoned as "lost to a${a.id}".` : 'No other open attempts on this task.',
      ]
      if (carried.length) lines.push(`${refs(carried)} will be promoted along with it.`)
      const detail = lines.join('\n')
      const ok = await vscode.window.showWarningMessage(
        `Promote a${a.id} as the winner?`,
        { modal: true, detail },
        'Promote',
      )
      if (ok !== 'Promote') return undefined
    }
    const r = await this.conn.backend.promote(a.id)
    const lost = r.closed_siblings.map((s) => `a${s}`).join(', ')
    this.done(r, `a${a.id} promoted${lost ? `; abandoned ${lost}` : ''}`, ['Open Pull Request'])
    return r
  }

  private async abandon(n?: Node, reason?: string) {
    const a = await this.attemptArg(n, (x) => isOpen(x.status))
    if (!a) return undefined
    const why =
      reason ??
      (await vscode.window.showInputBox({
        title: `Abandon a${a.id}`,
        prompt: 'Why? Kept in history so the next attempt can learn from it.',
        ignoreFocusOut: true,
        validateInput: (v) => (v.trim() ? undefined : 'A reason is required'),
      }))
    if (!why) return undefined
    const r = await this.conn.backend.abandon(a.id, why.trim())
    this.done(r, `a${a.id} abandoned`)
    return r
  }

  private async stopRun(n?: Node) {
    const a = await this.attemptArg(n)
    if (!a) return
    const r = await this.conn.backend.stop(a.id)
    void vscode.window.showInformationMessage(`agentvc: sent ${r.signal} to run ${r.run} of a${a.id}`)
    this.conn.poke()
  }

  private async openPr(n?: Node) {
    const a = await this.attemptArg(n)
    if (!a) return
    const out = (await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: `Opening a pull request for a${a.id}…` },
      () => this.conn.cli(['pr', `a${a.id}`]),
    )) as { pr: { url: string; number: number; updated: boolean } }
    const { pr } = out
    const choice = await vscode.window.showInformationMessage(
      `agentvc: ${pr.updated ? 'updated' : 'opened'} PR #${pr.number}`,
      'Open',
    )
    if (choice === 'Open') await vscode.env.openExternal(vscode.Uri.parse(pr.url))
  }

  private async openInWebUi(n?: Node) {
    const d = this.conn.daemon
    if (!d) throw new Error('the review UI needs agentvcd; run "agentvc: Start agentvcd"')
    let p = '/inbox'
    if (n?.kind === 'task') p = `/tasks/${n.task.id}`
    else if (n?.kind === 'attempt') p = `/attempts/${n.node.attempt.id}`
    else if (n?.kind === 'snapshot') p = `/attempts/${n.attempt.id}`
    await vscode.env.openExternal(vscode.Uri.parse(d.baseUrl + p))
  }

  private async copyRef(n: Node) {
    const ref = n.kind === 'task' ? `t${n.task.id}` : this.refOf(n)
    if (ref) await vscode.env.clipboard.writeText(ref)
  }

  private async reveal(id?: number) {
    if (id === undefined) return
    const node = await this.tree.nodeFor(id)
    if (node) await this.view.reveal(node, { select: true, focus: true, expand: true })
  }

  private done(r: ActionResponse, msg: string, actions: string[] = []) {
    this.conn.poke()
    void vscode.window.showInformationMessage(`agentvc: ${msg}`, ...actions).then((c) => {
      if (c === 'Open Pull Request') {
        void vscode.commands.executeCommand('agentvc.openPr', {
          kind: 'attempt',
          node: { attempt: r.attempt, children: [] },
        })
      }
    })
  }

  /** The attempt a command acts on: its tree node, the selection, or a pick. */
  private async attemptArg(n?: Node, filter: (a: Attempt) => boolean = () => true): Promise<Attempt | undefined> {
    if (n?.kind === 'attempt') return n.node.attempt
    const sel = this.view.selection[0]
    if (!n && sel?.kind === 'attempt') return sel.node.attempt
    if (!n && sel?.kind === 'snapshot') return sel.attempt
    const pool = this.tree.allAttempts.filter(filter)
    if (!pool.length) {
      void vscode.window.showInformationMessage('agentvc: no matching attempts')
      return undefined
    }
    const pick = await vscode.window.showQuickPick(
      [...pool]
        .sort((x, y) => y.id - x.id)
        .map((a) => ({ label: `a${a.id}`, description: `${a.status} · ${a.actor}`, attempt: a })),
      { placeHolder: 'Attempt' },
    )
    return pick?.attempt
  }
}
