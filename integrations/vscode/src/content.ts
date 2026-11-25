// Read-only documents for a file at a ref: `agentvc:/src/x.ts?a3`. Used as
// both sides of VS Code's native diff editor.

import * as vscode from 'vscode'
import type { Connection } from './connection'
import { errorText } from './connection'
import type { Ref } from './types'

export const SCHEME = 'agentvc'

export function fileUri(ref: Ref, path: string): vscode.Uri {
  return vscode.Uri.from({ scheme: SCHEME, path: `/${path}`, query: ref })
}

export function parseFileUri(uri: vscode.Uri): { ref: Ref; path: string } {
  return { ref: uri.query as Ref, path: uri.path.replace(/^\//, '') }
}

export class SnapshotContent implements vscode.TextDocumentContentProvider {
  private readonly _onDidChange = new vscode.EventEmitter<vscode.Uri>()
  readonly onDidChange = this._onDidChange.event

  constructor(private readonly conn: Connection) {}

  async provideTextDocumentContent(uri: vscode.Uri): Promise<string> {
    const { ref, path } = parseFileUri(uri)
    try {
      const f = await this.conn.backend.file(ref, path)
      if (!f) return ''
      if (f.too_large) return `(${path} is too large to show: ${f.size} bytes)`
      if (f.binary || f.content === null) return `(binary file, ${f.size} bytes)`
      return f.content
    } catch (e) {
      return `(could not read ${path} at ${ref}: ${errorText(e)})`
    }
  }

  /** An attempt's latest state moves as it snapshots; refresh open `aN` documents. */
  refreshAttemptDocs(): void {
    for (const doc of vscode.workspace.textDocuments) {
      if (doc.uri.scheme === SCHEME && doc.uri.query.startsWith('a')) this._onDidChange.fire(doc.uri)
    }
  }
}
