// The branch graph in the side bar and the review in an editor tab, both the
// daemon's own review UI in a frame (see reviewHtml.ts). Clicking something in
// the side bar opens it in the review tab.

import * as vscode from 'vscode'
import { machine, type Connection } from './connection'
import { WEBVIEW_COMMANDS, frameHtml, nonce, offlineHtml, refLabel, safePath, safeRef, type Surface } from './reviewHtml'
import { fileUri } from './content'
import type { Ref } from './types'

export const BRANCHES_VIEW = 'agentvc.branches'

type FromWebview = { type?: unknown; path?: unknown; command?: unknown; file?: unknown; line?: unknown; base?: unknown; head?: unknown; ref?: unknown }

export class ReviewHost implements vscode.WebviewViewProvider, vscode.Disposable {
  private view: vscode.WebviewView | undefined
  private panel: vscode.WebviewPanel | undefined
  private panelPath = '/review'
  private readonly subs: vscode.Disposable[] = []
  private shown: string | undefined

  constructor(private readonly conn: Connection) {
    // Reconnects repeat the same state; only a real change reloads the frames.
    this.subs.push(
      conn.onDidChangeState((s) => {
        if (s === this.shown) return
        this.shown = s
        void this.renderAll()
      }),
    )
  }

  register(ctx: vscode.ExtensionContext) {
    ctx.subscriptions.push(
      vscode.window.registerWebviewViewProvider(BRANCHES_VIEW, this, { webviewOptions: { retainContextWhenHidden: true } }),
      vscode.commands.registerCommand('agentvc.openReview', (path?: unknown) => this.openPanel(typeof path === 'string' ? path : '/review')),
      this,
    )
  }

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view
    view.webview.options = { enableScripts: true }
    view.webview.onDidReceiveMessage((m: FromWebview) => this.onMessage(m), undefined, this.subs)
    view.onDidDispose(() => (this.view = undefined), undefined, this.subs)
    void this.render(view.webview, 'sidebar', '/review')
  }

  /** Show `path` (like `/review/v/3`) in the review tab, opening it if needed. */
  async openPanel(path: string): Promise<void> {
    this.panelPath = path.startsWith('/') ? path : '/review'
    if (this.panel) {
      this.panel.reveal(undefined, false)
      void this.panel.webview.postMessage({ type: 'agentvc.navigate', path: this.panelPath })
      return
    }
    const panel = vscode.window.createWebviewPanel('agentvc.review', 'agentvc Review', vscode.ViewColumn.Active, {
      enableScripts: true,
      retainContextWhenHidden: true,
    })
    this.panel = panel
    panel.webview.onDidReceiveMessage((m: FromWebview) => this.onMessage(m), undefined, this.subs)
    panel.onDidDispose(() => (this.panel = undefined), undefined, this.subs)
    await this.render(panel.webview, 'panel', this.panelPath)
  }

  private onMessage(m: FromWebview) {
    if (m.type === 'agentvc.open' && typeof m.path === 'string') void this.openPanel(m.path)
    else if (m.type === 'agentvc.openFile' && typeof m.file === 'string') void openFile(m.file, typeof m.line === 'number' ? m.line : undefined)
    else if (m.type === 'agentvc.openDiff') void openDiff(m.base, m.head, m.path)
    else if (m.type === 'agentvc.openRef') void openRef(m.ref, m.path)
    else if (m.type === 'agentvc.command' && typeof m.command === 'string' && (WEBVIEW_COMMANDS as readonly string[]).includes(m.command))
      void vscode.commands.executeCommand(m.command)
  }

  private async renderAll() {
    if (this.view) await this.render(this.view.webview, 'sidebar', '/review')
    if (this.panel) await this.render(this.panel.webview, 'panel', this.panelPath)
  }

  private async render(webview: vscode.Webview, surface: Surface, path: string) {
    const d = this.conn.daemon
    const n = nonce()
    if (!d) {
      webview.html = offlineHtml({
        state: this.conn.state,
        machine: machine(),
        folder: this.conn.workspaceDir(),
        cspSource: webview.cspSource,
        nonce: n,
      })
      return
    }
    // Remote windows (SSH, containers) reach the daemon through a forwarded port.
    const ext = await vscode.env.asExternalUri(vscode.Uri.parse(d.baseUrl))
    const base = ext.toString(true).replace(/\/+$/, '')
    const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath
    webview.html = frameHtml({ base, path, surface, folder, cspSource: webview.cspSource, nonce: n })
  }

  /** The review tab's page, for tests. */
  get panelHtml(): string | undefined {
    return this.panel?.webview.html
  }

  dispose() {
    this.subs.forEach((s) => s.dispose())
    this.subs.length = 0
    this.panel?.dispose()
  }
}

/**
 * Open a file the review UI points at (Code tab, diffs) in an editor, at a
 * 1-based line. Only absolute paths of existing files: the page is a frame
 * of the daemon and is not trusted with anything else.
 */
export async function openFile(file: string, line?: number): Promise<boolean> {
  if (!/^(\/|[A-Za-z]:[\\/])/.test(file)) return false
  const uri = vscode.Uri.file(file)
  try {
    const stat = await vscode.workspace.fs.stat(uri)
    if (stat.type !== vscode.FileType.File) return false
  } catch {
    void vscode.window.showWarningMessage(`agentvc: ${file} is not on this machine.`)
    return false
  }
  const at = line && line > 0 ? new vscode.Position(line - 1, 0) : undefined
  await vscode.window.showTextDocument(uri, { preview: false, selection: at ? new vscode.Range(at, at) : undefined })
  return true
}

/**
 * Show a file's change from the review UI in the editor's own diff view. The
 * sides are read-only `agentvc:` documents read through the daemon, so this
 * works for branches, agents and commits that are not checked out here.
 */
export async function openDiff(base: unknown, head: unknown, path: unknown): Promise<boolean> {
  // Checked by safeRef: one of the ref forms the daemon's /file reads.
  const a = safeRef(base) as Ref | null
  const b = safeRef(head) as Ref | null
  const p = safePath(path)
  if (!a || !b || !p) return false
  await vscode.commands.executeCommand('vscode.diff', fileUri(a, p), fileUri(b, p), `${p} (${refLabel(a)} ↔ ${refLabel(b)})`, { preview: true })
  return true
}

/** Open a file as it is at a ref, read-only, when it is not on this machine. */
export async function openRef(ref: unknown, path: unknown): Promise<boolean> {
  const r = safeRef(ref) as Ref | null
  const p = safePath(path)
  if (!r || !p) return false
  await vscode.window.showTextDocument(fileUri(r, p), { preview: true })
  return true
}
