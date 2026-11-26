import * as vscode from 'vscode'
import { Commands } from './commands'
import { Connection } from './connection'
import { ReviewHost } from './review'
import { SCHEME, SnapshotContent } from './content'
import { StackView } from './stack'
import { StatusBar } from './statusBar'
import { AttemptsTree } from './tree'

/** Returned from `activate`, for tests and other extensions. */
export interface AgentvcApi {
  connection: Connection
  tree: AttemptsTree
  commands: Commands
  statusBar: StatusBar
  stack: StackView
  review: ReviewHost
  /** Resolves after the next tree reload. */
  whenRefreshed(): Promise<void>
}

export async function activate(ctx: vscode.ExtensionContext): Promise<AgentvcApi> {
  const log = vscode.window.createOutputChannel('agentvc')
  const conn = new Connection(log)
  // The attempt and stack models back the commands and the status bar; the
  // one tree people see is the Branches view (the shared review UI).
  const tree = new AttemptsTree(conn)
  const content = new SnapshotContent(conn)
  const statusBar = new StatusBar(conn, tree)
  const commands = new Commands(conn, tree)
  commands.register(ctx)
  // The branch graph and review: the same UI as the browser, in a frame.
  const review = new ReviewHost(conn)
  review.register(ctx)
  // This window's own stack (#107): CLI only, no space or daemon needed.
  const stack = new StackView(log)
  stack.register(ctx)
  ctx.subscriptions.push(stack)
  void stack.reload()

  const refreshed = new vscode.EventEmitter<void>()
  const refresh = async () => {
    await tree.reload()
    await statusBar.update()
    refreshed.fire()
  }

  ctx.subscriptions.push(
    log,
    conn,
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
    stack,
    review,
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
