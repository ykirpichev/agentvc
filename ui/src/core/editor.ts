// "Open in editor" (#173): inside VS Code or Cursor the host opens the file;
// in a browser, a vscode:// or cursor:// link does (the daemon's folders are
// on this machine for a local daemon).

import { embedMode, toHost } from './embed'

export type Editor = 'vscode' | 'cursor'

const KEY = 'agentvc.editor'

export function preferredEditor(): Editor {
  try {
    return localStorage.getItem(KEY) === 'cursor' ? 'cursor' : 'vscode'
  } catch {
    return 'vscode'
  }
}

export function setPreferredEditor(e: Editor) {
  try {
    localStorage.setItem(KEY, e)
  } catch {
    // No storage: the choice lasts for this page only.
  }
}

export const EDITOR_NAMES: Record<Editor, string> = { vscode: 'VS Code', cursor: 'Cursor' }

/** The absolute file for `path` inside `folder`, with forward slashes. */
export function joinPath(folder: string, path: string): string {
  const f = folder.replace(/\\/g, '/').replace(/\/+$/, '')
  return path ? `${f}/${path.replace(/^\/+/, '')}` : f
}

/** A `vscode://file/...:line` (or cursor://) link. */
export function editorUrl(editor: Editor, file: string, line?: number): string {
  const abs = file.startsWith('/') ? file : `/${file}`
  const enc = abs.split('/').map(encodeURIComponent).join('/').replace(/^\/([A-Za-z])%3A/, '/$1:')
  return `${editor}://file${enc}${line ? `:${line}` : ''}`
}

/** Whether the host editor opens files itself (the review UI runs inside it). */
export const inEditor = embedMode !== null

/** Open `file` at `line`: through the host when embedded, else with a link. */
export function openInEditor(file: string, line?: number, editor: Editor = preferredEditor()) {
  if (inEditor) {
    toHost({ type: 'agentvc.openFile', file, line })
    return
  }
  window.location.href = editorUrl(editor, file, line)
}

/** Show a file's change in the host editor's own diff view (embedded only). */
export function openDiffInEditor(base: string, head: string, path: string) {
  toHost({ type: 'agentvc.openDiff', base, head, path })
}

/**
 * Open a file in the host editor: the file on disk when it is on this
 * machine, else the version at `ref`, read-only (embedded only).
 */
export function openFileInEditor(ref: string, path: string, localFile?: string) {
  if (localFile) toHost({ type: 'agentvc.openFile', file: localFile })
  else toHost({ type: 'agentvc.openRef', ref, path })
}
