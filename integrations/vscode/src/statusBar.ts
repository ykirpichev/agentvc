// Status bar: the attempt whose view is open in this window, and its last snapshot.
// Shown only in such a window (and while the daemon is down).

import * as vscode from 'vscode'
import type { Connection } from './connection'
import { ago, attemptForPath } from './model'
import type { AttemptsTree } from './tree'
import type { Attempt, Snapshot } from './types'

export class StatusBar implements vscode.Disposable {
  private readonly item = vscode.window.createStatusBarItem('agentvc.current', vscode.StatusBarAlignment.Left, 50)
  /** The attempt this window's workspace is a view of. */
  current: { attempt: Attempt; head: Snapshot | null } | undefined

  constructor(
    private readonly conn: Connection,
    private readonly tree: AttemptsTree,
  ) {
    this.item.name = 'agentvc'
  }

  /** `folders` defaults to the workspace folders. */
  async update(folders = (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath)): Promise<void> {
    const s = this.conn.state
    if (s === 'nospace') {
      this.item.hide()
      return
    }
    if (s === 'offline' || s === 'connecting') {
      this.current = undefined
      this.item.text = s === 'offline' ? '$(debug-disconnect) agentvc' : '$(loading~spin) agentvc'
      this.item.tooltip = s === 'offline' ? 'agentvcd is not running. Click to start it.' : 'Connecting…'
      this.item.command = s === 'offline' ? 'agentvc.startDaemon' : undefined
      this.item.show()
      return
    }
    let attempt: Attempt | undefined
    for (const f of folders) {
      attempt = attemptForPath(f, this.tree.allAttempts.filter((a) => a.parent_attempt !== null))
      if (attempt) break
    }
    if (!attempt) {
      // Outside an attempt's view the stack item says where you are; open
      // attempts and tasks are history, so they don't get a status bar entry.
      this.current = undefined
      this.item.hide()
      return
    }
    let head: Snapshot | null = null
    try {
      head = (await this.conn.backend.attempt(attempt.id)).head
    } catch {
      /* keep the attempt, without its head */
    }
    this.current = { attempt, head }
    const live = this.tree.liveOf(attempt.id)
    const icon = live?.run?.alive ? '$(sync~spin)' : '$(versions)'
    this.item.text = `${icon} a${attempt.id}${head ? ` · s${head.id} ${ago(head.created_at)}` : ''}`
    const md = new vscode.MarkdownString(undefined, true)
    md.appendMarkdown(`**a${attempt.id}** · ${attempt.status} · \`${attempt.actor}\`\n\n`)
    const task = this.tree.allTasks.find((t) => t.id === attempt.task_id)
    if (task) md.appendMarkdown(`t${task.id} ${task.goal}\n\n`)
    if (head) md.appendMarkdown(`last snapshot s${head.id} (${head.trigger}), ${new Date(head.created_at * 1000).toLocaleString()}`)
    this.item.tooltip = md
    this.item.command = { command: 'agentvc.revealCurrent', title: 'Reveal', arguments: [attempt.id] }
    this.item.show()
  }

  dispose(): void {
    this.item.dispose()
  }
}
