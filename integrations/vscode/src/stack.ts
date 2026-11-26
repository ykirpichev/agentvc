// The Stack view: this window's stacked branches (`agentvc stack`, #107),
// independent of every other window on the same repository. Runs the CLI in
// the workspace folder, so it needs no agentvc space and no daemon.
//
// Drag a branch onto another (or onto the trunk row) to move it there; the
// branches above it come along and only its own commits move. Other windows'
// branches show read-only (locked) and are valid drop targets.

import { execFile } from 'node:child_process'
import * as path from 'node:path'
import * as vscode from 'vscode'
import { AgentvcError, cliRunner } from './client'
import {
  branchDescription,
  canMoveOnto,
  githubRepo,
  position,
  stackForest,
  staleCount,
  type OtherStack,
  type StackLog,
  type StackNode,
} from './stackModel'

export type StackItem =
  | { kind: 'trunk'; log: StackLog }
  | { kind: 'branch'; node: StackNode }
  | { kind: 'others'; others: OtherStack[] }
  | { kind: 'otherWindow'; other: OtherStack }
  | { kind: 'otherBranch'; name: string; parent: string; pr?: number; window: string }

const MIME = 'application/vnd.code.tree.agentvc.stack'

function cfg() {
  const c = vscode.workspace.getConfiguration('agentvc')
  return {
    cliPath: c.get<string>('cliPath', 'agentvc') || 'agentvc',
    showOthers: c.get<boolean>('stack.showOtherWindows', true),
  }
}

function git(cwd: string, args: string[]): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile('git', args, { cwd }, (err, stdout, stderr) => {
      if (err) reject(new AgentvcError('git', (stderr || String(err)).trim()))
      else resolve(stdout.trim())
    })
  })
}

export class StackView
  implements vscode.TreeDataProvider<StackItem>, vscode.TreeDragAndDropController<StackItem>, vscode.Disposable
{
  readonly dragMimeTypes = [MIME]
  readonly dropMimeTypes = [MIME]
  log: StackLog | undefined
  error: AgentvcError | undefined
  private readonly changed = new vscode.EventEmitter<StackItem | undefined>()
  readonly onDidChangeTreeData = this.changed.event
  private readonly status = vscode.window.createStatusBarItem('agentvc.stack', vscode.StatusBarAlignment.Left, 49)
  private watcher: vscode.FileSystemWatcher | undefined
  private watchedDir: string | undefined
  private debounce: NodeJS.Timeout | undefined
  private loading: Promise<void> | undefined

  constructor(private readonly out: vscode.OutputChannel) {
    this.status.name = 'agentvc stack'
  }

  /** The worktree this window works in: the first workspace folder. */
  get folder(): string | undefined {
    return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath
  }

  private run(args: string[]): Promise<Record<string, unknown>> {
    const dir = this.folder
    if (!dir) return Promise.reject(new AgentvcError('no_folder', 'open a folder first'))
    this.out.appendLine(`agentvc stack ${args.join(' ')}`)
    return cliRunner(cfg().cliPath, dir)(['stack', ...args])
  }

  reload(): Promise<void> {
    this.loading ??= this.load().finally(() => (this.loading = undefined))
    return this.loading
  }

  private async load(): Promise<void> {
    let state = 'ready'
    try {
      this.log = (await this.run(cfg().showOthers ? ['log', '--all'] : ['log'])) as unknown as StackLog
      this.error = undefined
      if (!this.log.branches.length) state = 'empty'
      this.watch(this.log.git_dir)
    } catch (e) {
      this.log = undefined
      this.error = e instanceof AgentvcError ? e : new AgentvcError('cli', String(e))
      state = this.error.code === 'not_git_repo' || this.error.code === 'no_folder' ? 'nogit' : 'error'
    }
    void vscode.commands.executeCommand('setContext', 'agentvc.stack.state', state)
    this.updateStatus()
    this.changed.fire(undefined)
  }

  /** Reload when this window's stack file or HEAD changes (commands, git, the terminal). */
  private watch(gitDir: string) {
    if (gitDir === this.watchedDir) return
    this.watcher?.dispose()
    this.watchedDir = gitDir
    this.watcher = vscode.workspace.createFileSystemWatcher(
      new vscode.RelativePattern(vscode.Uri.file(gitDir), '{HEAD,agentvc/stack.json}'),
    )
    const soon = () => {
      clearTimeout(this.debounce)
      this.debounce = setTimeout(() => void this.reload(), 300)
    }
    this.watcher.onDidChange(soon)
    this.watcher.onDidCreate(soon)
    this.watcher.onDidDelete(soon)
  }

  private updateStatus() {
    const log = this.log
    if (!log || (!log.branches.length && !log.pending)) {
      this.status.hide()
      return
    }
    const pos = position(log)
    const stale = staleCount(log)
    let text = pos ? `$(layers) ${pos.branch} ${pos.index}/${pos.total}` : `$(layers) ${log.branches.length} stacked`
    if (log.pending) text += ' · $(warning) rebase in progress'
    else if (stale) text += ` · $(warning) ${stale} to restack`
    this.status.text = text
    this.status.tooltip = `This window's stack on ${log.base_ref}${stale ? `: ${stale} branch${stale === 1 ? '' : 'es'} need a restack` : ''}`
    this.status.command = 'agentvc.stack.focus'
    this.status.show()
  }

  // ---- tree ----

  getChildren(item?: StackItem): StackItem[] {
    const log = this.log
    if (!log) return []
    if (!item) {
      const rows: StackItem[] = [{ kind: 'trunk', log }]
      for (const n of stackForest(log)) rows.push({ kind: 'branch', node: n })
      const others = (log.others ?? []).filter((o) => o.branches.length)
      if (others.length) rows.push({ kind: 'others', others })
      return rows
    }
    switch (item.kind) {
      case 'branch':
        return item.node.children.map((node) => ({ kind: 'branch', node }))
      case 'others':
        return item.others.map((other) => ({ kind: 'otherWindow', other }))
      case 'otherWindow':
        return item.other.branches.map((b) => ({
          kind: 'otherBranch',
          name: b.name,
          parent: b.parent,
          pr: b.pr,
          window: item.other.path,
        }))
      default:
        return []
    }
  }

  getTreeItem(item: StackItem): vscode.TreeItem {
    switch (item.kind) {
      case 'trunk': {
        const t = new vscode.TreeItem(item.log.base_ref, vscode.TreeItemCollapsibleState.None)
        t.description = item.log.base?.slice(0, 10) ?? 'missing'
        t.iconPath = new vscode.ThemeIcon('git-commit')
        t.tooltip = 'The trunk this window stacks on. Drop a branch here to put it on the trunk.'
        t.contextValue = 'stackTrunk'
        return t
      }
      case 'branch': {
        const b = item.node.branch
        const t = new vscode.TreeItem(
          b.name,
          item.node.children.length
            ? vscode.TreeItemCollapsibleState.Expanded
            : vscode.TreeItemCollapsibleState.None,
        )
        t.id = `branch:${b.name}`
        t.description = branchDescription(b, this.log!)
        t.iconPath = b.current
          ? new vscode.ThemeIcon('circle-filled', new vscode.ThemeColor('charts.blue'))
          : b.needs_restack
            ? new vscode.ThemeIcon('warning', new vscode.ThemeColor('list.warningForeground'))
            : new vscode.ThemeIcon('git-branch')
        const md = new vscode.MarkdownString(undefined, true)
        md.appendMarkdown(`**${b.name}**${b.current ? ' (checked out)' : ''}\n\n`)
        md.appendMarkdown(`${b.commits} commit${b.commits === 1 ? '' : 's'} on \`${b.parent}\``)
        if (b.pr) md.appendMarkdown(` · PR #${b.pr}`)
        if (b.needs_restack) md.appendMarkdown('\n\nIts parent moved: restack to bring it up to date.')
        md.appendMarkdown('\n\nDrag onto another branch or the trunk to move it.')
        t.tooltip = md
        t.contextValue = `stackBranch${b.current ? ':current' : ''}${b.pr ? ':pr' : ''}`
        if (!b.current) {
          t.command = { command: 'agentvc.stack.checkout', title: 'Check Out', arguments: [item] }
        }
        return t
      }
      case 'others': {
        const t = new vscode.TreeItem('Other windows', vscode.TreeItemCollapsibleState.Collapsed)
        t.iconPath = new vscode.ThemeIcon('lock')
        t.description = 'read-only'
        t.tooltip = 'Stacks of the other worktrees of this repository. Change them in their own window.'
        return t
      }
      case 'otherWindow': {
        const t = new vscode.TreeItem(path.basename(item.other.path), vscode.TreeItemCollapsibleState.Expanded)
        t.description = item.other.path
        t.iconPath = new vscode.ThemeIcon('window')
        return t
      }
      case 'otherBranch': {
        const t = new vscode.TreeItem(item.name, vscode.TreeItemCollapsibleState.None)
        t.description = `on ${item.parent}${item.pr ? ` · #${item.pr}` : ''}`
        t.iconPath = new vscode.ThemeIcon('lock')
        t.tooltip = `Belongs to the window at ${item.window}. You can drop a branch of this window onto it to build on it; it is never rewritten from here.`
        t.contextValue = `stackOther${item.pr ? ':pr' : ''}`
        return t
      }
    }
  }

  // ---- drag to move ----

  handleDrag(source: readonly StackItem[], data: vscode.DataTransfer): void {
    const names = source.flatMap((s) => (s.kind === 'branch' ? [s.node.branch.name] : []))
    if (names.length === 1) data.set(MIME, new vscode.DataTransferItem(names[0]))
  }

  async handleDrop(target: StackItem | undefined, data: vscode.DataTransfer): Promise<void> {
    const branch = (await data.get(MIME)?.asString())?.trim()
    if (!branch || !target || !this.log) return
    const onto =
      target.kind === 'trunk'
        ? this.log.trunk
        : target.kind === 'branch'
          ? target.node.branch.name
          : target.kind === 'otherBranch'
            ? target.name
            : undefined
    if (!onto) return
    await this.moveOnto(branch, onto)
  }

  async moveOnto(branch: string, onto: string): Promise<void> {
    if (!this.log || !canMoveOnto(this.log, branch, onto)) {
      void vscode.window.showWarningMessage(`Can't move ${branch} onto ${onto}: it is ${branch} or stacked on it.`)
      return
    }
    const current = this.log.branches.find((b) => b.name === branch)
    if (current?.parent === onto) return
    await this.act(`Moving ${branch} onto ${onto}…`, ['move', branch, '--onto', onto])
  }

  // ---- actions ----

  /** Run a stack command with progress; a conflict offers to resolve in the editor. */
  async act(title: string, args: string[]): Promise<Record<string, unknown> | undefined> {
    try {
      const r = await vscode.window.withProgress(
        { location: vscode.ProgressLocation.Window, title },
        () => this.run(args),
      )
      return r
    } catch (e) {
      const err = e instanceof AgentvcError ? e : new AgentvcError('cli', String(e))
      if (err.code === 'conflict') {
        const pick = await vscode.window.showWarningMessage(err.message, 'Resolve in Editor')
        if (pick) {
          await this.act('Restacking…', ['restack', '--keep-conflicts'])
          void vscode.commands.executeCommand('workbench.view.scm')
        }
      } else {
        void vscode.window.showErrorMessage(`agentvc stack: ${err.message}`)
      }
      return undefined
    } finally {
      await this.reload()
    }
  }

  private nameOf(item: StackItem | undefined): string | undefined {
    if (item?.kind === 'branch') return item.node.branch.name
    if (item?.kind === 'otherBranch') return item.name
    return this.log?.current ?? undefined
  }

  private prOf(item: StackItem | undefined): number | undefined {
    if (item?.kind === 'branch') return item.node.branch.pr
    if (item?.kind === 'otherBranch') return item.pr
    return this.log?.branches.find((b) => b.current)?.pr
  }

  register(ctx: vscode.ExtensionContext, view: vscode.TreeView<StackItem>): void {
    const reg = (id: string, fn: (...a: never[]) => unknown) =>
      ctx.subscriptions.push(vscode.commands.registerCommand(id, fn))
    reg('agentvc.stack.refresh', () => this.reload())
    reg('agentvc.stack.focus', () => vscode.commands.executeCommand('agentvc.stack.view.focus'))
    reg('agentvc.stack.create', async () => {
      const message = await vscode.window.showInputBox({
        title: 'New stacked branch',
        prompt: 'Commit message for your changes (all changes are staged); empty for a branch without a commit',
      })
      if (message === undefined) return
      const name = await vscode.window.showInputBox({
        title: 'New stacked branch',
        prompt: 'Branch name',
        value: message ? undefined : '',
        placeHolder: message ? 'leave empty to name it after the message' : 'branch name',
      })
      if (name === undefined || (!name && !message)) return
      const args = ['create', ...(name ? [name] : []), ...(message ? ['-a', '-m', message] : [])]
      await this.act('Creating branch…', args)
    })
    reg('agentvc.stack.modify', () => this.act('Amending…', ['modify', '-a']))
    reg('agentvc.stack.commit', async () => {
      const m = await vscode.window.showInputBox({ title: 'Add a commit to this branch', prompt: 'Commit message' })
      if (m) await this.act('Committing…', ['modify', '-a', '-c', '-m', m])
    })
    reg('agentvc.stack.restack', () => this.act('Restacking…', ['restack']))
    reg('agentvc.stack.sync', () => this.act('Syncing with the trunk…', ['sync']))
    reg('agentvc.stack.submit', async () => {
      const r = await this.act('Pushing and updating PRs…', ['submit'])
      const prs = (r?.prs as { number: number; created: boolean }[] | undefined) ?? []
      if (r) {
        const made = prs.filter((p) => p.created).length
        void vscode.window.showInformationMessage(
          `Submitted ${prs.length} PR${prs.length === 1 ? '' : 's'}${made ? ` (${made} new)` : ''}.`,
        )
      }
    })
    for (const dir of ['up', 'down', 'top', 'bottom']) {
      reg(`agentvc.stack.${dir}`, () => this.act(`Checking out ${dir}…`, [dir]))
    }
    reg('agentvc.stack.checkout', async (item?: StackItem) => {
      const name = this.nameOf(item)
      if (!name || !this.folder) return
      try {
        await git(this.folder, ['switch', name])
      } catch (e) {
        void vscode.window.showErrorMessage(`git switch ${name}: ${(e as Error).message}`)
      }
      await this.reload()
    })
    reg('agentvc.stack.moveOnto', async (item?: StackItem) => {
      const branch = this.nameOf(item)
      if (!branch || !this.log) return
      const log = this.log
      const targets = [
        { label: log.base_ref, description: 'the trunk', onto: log.trunk },
        ...log.branches
          .filter((b) => canMoveOnto(log, branch, b.name))
          .map((b) => ({ label: b.name, description: branchDescription(b, log), onto: b.name })),
        ...(log.others ?? []).flatMap((o) =>
          o.branches.map((b) => ({
            label: b.name,
            description: `read-only, window ${path.basename(o.path)}`,
            onto: b.name,
          })),
        ),
      ]
      const pick = await vscode.window.showQuickPick(targets, { title: `Move ${branch} onto…` })
      if (pick) await this.moveOnto(branch, pick.onto)
    })
    reg('agentvc.stack.adopt', async () => {
      if (!this.folder) return
      const tracked = new Set(this.log?.branches.map((b) => b.name))
      const owned = new Set((this.log?.others ?? []).flatMap((o) => o.branches.map((b) => b.name)))
      const all = (await git(this.folder, ['for-each-ref', '--format=%(refname:short)', 'refs/heads']).catch(() => ''))
        .split('\n')
        .filter((b) => b && !tracked.has(b) && !owned.has(b) && b !== this.log?.trunk)
      if (!all.length) {
        void vscode.window.showInformationMessage('No untracked local branches.')
        return
      }
      const pick = await vscode.window.showQuickPick(all, {
        title: 'Track a branch in this window (and the untracked branches below it)',
      })
      if (pick) await this.act(`Adopting ${pick}…`, ['adopt', pick])
    })
    reg('agentvc.stack.rename', async (item?: StackItem) => {
      const name = this.nameOf(item)
      if (!name) return
      const to = await vscode.window.showInputBox({ title: `Rename ${name}`, value: name })
      if (to && to !== name) await this.act(`Renaming ${name}…`, ['rename', name, to])
    })
    reg('agentvc.stack.delete', async (item?: StackItem) => {
      const name = this.nameOf(item)
      if (!name) return
      const ok = await vscode.window.showWarningMessage(
        `Delete ${name} and its commits? Branches on it keep only their own commits.`,
        { modal: true },
        'Delete',
      )
      if (ok) await this.act(`Deleting ${name}…`, ['delete', name])
    })
    reg('agentvc.stack.squash', () => this.act('Squashing…', ['squash']))
    reg('agentvc.stack.untrack', async (item?: StackItem) => {
      const name = this.nameOf(item)
      if (name) await this.act(`Untracking ${name}…`, ['untrack', name])
    })
    reg('agentvc.stack.openPr', async (item?: StackItem) => {
      const pr = this.prOf(item)
      if (!pr || !this.folder) return
      const url = await git(this.folder, ['remote', 'get-url', 'origin']).catch(() => '')
      const repo = githubRepo(url)
      if (repo) void vscode.env.openExternal(vscode.Uri.parse(`https://github.com/${repo}/pull/${pr}`))
    })
    ctx.subscriptions.push(
      view,
      vscode.window.onDidChangeWindowState((s) => {
        // Another window may have moved a branch this one builds on.
        if (s.focused) void this.reload()
      }),
      vscode.workspace.onDidChangeWorkspaceFolders(() => void this.reload()),
      vscode.workspace.onDidChangeConfiguration((e) => {
        if (e.affectsConfiguration('agentvc')) void this.reload()
      }),
    )
  }

  dispose(): void {
    clearTimeout(this.debounce)
    this.watcher?.dispose()
    this.status.dispose()
    this.changed.dispose()
  }
}
