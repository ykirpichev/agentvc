import * as vscode from 'vscode'
import { Commands } from './commands'
import { Connection } from './connection'
import { SCHEME, SnapshotContent } from './content'
import { StatusBar } from './statusBar'
import { AttemptsTree, type Node } from './tree'

/** Returned from `activate`, for tests and other extensions. */
export interface AgentvcApi {
  connection: Connection
  tree: AttemptsTree
  commands: Commands
  statusBar: StatusBar
  /** Resolves after the next tree reload. */
  whenRefreshed(): Promise<void>
}

export async function activate(ctx: vscode.ExtensionContext): Promise<AgentvcApi> {
  const log = vscode.window.createOutputChannel('agentvc')
  const conn = new Connection(log)
  const tree = new AttemptsTree(conn)
  const view = vscode.window.createTreeView<Node>('agentvc.attempts', {
    treeDataProvider: tree,
    showCollapseAll: true,
  })
  const content = new SnapshotContent(conn)
  const statusBar = new StatusBar(conn, tree)
  const commands = new Commands(conn, tree, view)
  commands.register(ctx)

  const refreshed = new vscode.EventEmitter<void>()
  const refresh = async () => {
    await tree.reload()
    await statusBar.update()
    const t = tree.allTasks.filter((x) => x.attempts.passed > 0 && x.attempts.promoted === 0).length
    view.badge = t ? { value: t, tooltip: `${t} task${t === 1 ? '' : 's'} ready to pick a winner` } : undefined
    refreshed.fire()
  }

  ctx.subscriptions.push(
    log,
    conn,
    view,
    statusBar,
    refreshed,
    vscode.workspace.registerTextDocumentContentProvider(SCHEME, content),
    conn.onDidChange((ev) => {
      if (!ev || ev.type === 'snapshot_created' || ev.type === 'resync') content.refreshAttemptDocs()
      void refresh()
    }),
    conn.onDidChangeState(() => void refresh()),
    vscode.workspace.onDidChangeWorkspaceFolders(() => void conn.connect()),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('agentvc')) void conn.connect()
    }),
  )

  void conn.connect()
  return {
    connection: conn,
    tree,
    commands,
    statusBar,
    whenRefreshed: () => new Promise((resolve) => {
      const d = refreshed.event(() => {
        d.dispose()
        resolve()
      })
    }),
  }
}

export function deactivate(): void {
  /* disposables are cleaned up via the context */
}
