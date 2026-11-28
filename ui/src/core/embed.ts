// The review UI inside an editor (VS Code, Cursor) or the Claude app. The host
// loads it in a frame with `?embed=sidebar` (just the branch graph) or
// `?embed=panel` (just the review pane), and may pass `&theme=light|dark`
// and `&here=<folder>`, the editor window's folder (its workspace), and
// `&bg=<color>`, the editor's background there, so the frame blends in.
// In the sidebar, opening anything asks the host to show it in its review
// tab instead of navigating the narrow frame.

import type { WorkspaceInfo, WorkspaceList } from '../api/types'

export type EmbedMode = 'sidebar' | 'panel' | null

const KEY = 'agentvc.embed'

/** A plain CSS color, nothing that could carry other CSS. */
export const cssColor = (c: string | null | undefined): string | null =>
  c && /^(#[0-9a-f]{3,8}|rgba?\([\d.,% ]+\))$/i.test(c.trim()) ? c.trim() : null

function read(): { mode: EmbedMode; theme: 'light' | 'dark' | null; here: string | null; bg: string | null } {
  const q = new URLSearchParams(window.location.search)
  let mode = q.get('embed')
  let theme = q.get('theme')
  let here = q.get('here')
  let bg = q.get('bg')
  try {
    // Survives a reload inside the frame, where the query is gone.
    if (mode) sessionStorage.setItem(KEY, JSON.stringify({ mode, theme, here, bg }))
    else {
      const saved = JSON.parse(sessionStorage.getItem(KEY) ?? 'null') as { mode?: string; theme?: string; here?: string; bg?: string } | null
      mode = saved?.mode ?? null
      theme = theme ?? saved?.theme ?? null
      here = here ?? saved?.here ?? null
      bg = bg ?? saved?.bg ?? null
    }
  } catch {
    // No storage; the query alone decides.
  }
  return {
    mode: mode === 'sidebar' || mode === 'panel' ? mode : null,
    theme: theme === 'light' || theme === 'dark' ? theme : null,
    here,
    bg: cssColor(bg),
  }
}

const initial = read()

export const embedMode: EmbedMode = initial.mode
export const hostTheme = initial.theme
/** The editor window's folder, so "This workspace" means that window. */
export const hostFolder = initial.here
export const hostBg = initial.bg

/** Messages the page sends to its host. */
export type ToHost =
  | { type: 'agentvc.open'; path: string }
  | { type: 'agentvc.ready'; mode: EmbedMode }
  /** Open a file of this machine in the editor, at a line (1-based). */
  | { type: 'agentvc.openFile'; file: string; line?: number }

/** Messages a host sends to the page. */
export type FromHost =
  | {
      type: 'agentvc.theme'
      theme: 'light' | 'dark'
      bg?: string
      vars?: Record<string, string>
    }
  | { type: 'agentvc.navigate'; path: string }

/** A value of one editor theme variable, or null when it is not plain. */
export function hostVar(name: string, value: unknown): string | null {
  if (typeof value !== 'string' || !/^[A-Za-z][\w-]*$/.test(name)) return null
  const v = value.trim()
  if (name === 'font-family') return /^[\w\s"',.-]{1,300}$/.test(v) ? v : null
  if (name === 'font-size') return /^\d{1,2}(\.\d+)?px$/.test(v) ? v : null
  if (name === 'font-weight') return /^(normal|bold|\d{3})$/.test(v) ? v : null
  return cssColor(v)
}

/** Our tokens that follow an editor variable, when the editor has it. */
const TOKENS: [string, string[]][] = [
  ['--font-sans', ['font-family']],
  ['--text', ['foreground']],
  ['--text-2', ['foreground']],
  ['--muted', ['descriptionForeground']],
  ['--accent', ['button-background']],
  ['--accent-text', ['textLink-foreground']],
  ['--border', ['widget-border', 'panel-border']],
  ['--border-strong', ['input-border', 'dropdown-border', 'panel-border']],
  ['--hover', ['list-hoverBackground']],
  ['--surface-2', ['textCodeBlock-background', 'editorWidget-background']],
]

let applied: string[] = []

/**
 * Take on the editor's theme: its variables as `--vscode-*` (for the native
 * controls in native.css) and our own tokens where the editor has a value.
 * Each call replaces the last one's, so a theme switch leaves nothing stale.
 */
export function applyHostVars(vars: Record<string, string> | undefined, root: HTMLElement = document.documentElement) {
  for (const p of applied) root.style.removeProperty(p)
  applied = []
  const clean: Record<string, string> = {}
  for (const [k, v] of Object.entries(vars ?? {})) {
    const ok = hostVar(k, v)
    if (ok) clean[k] = ok
  }
  const set = (p: string, v: string) => {
    root.style.setProperty(p, v)
    applied.push(p)
  }
  for (const [k, v] of Object.entries(clean)) set(`--vscode-${k}`, v)
  // Cards and menus sit on the editor's own background, flat like its settings.
  const ground = embedMode === 'sidebar' ? ['sideBar-background', 'editor-background'] : ['editor-background']
  for (const [token, from] of [...TOKENS, ['--surface', ground] as [string, string[]], ['--bg', ground] as [string, string[]]]) {
    const hit = from.map((k) => clean[k]).find(Boolean)
    if (hit) set(token, hit)
  }
  if (Object.keys(clean).length) root.dataset.native = ''
  else delete root.dataset.native
}

export function toHost(msg: ToHost) {
  if (window.parent !== window) window.parent.postMessage(msg, '*')
}

/** Listen for host messages; returns the unsubscribe. */
export function onHost(fn: (msg: FromHost) => void): () => void {
  const on = (e: MessageEvent) => {
    if (e.source !== window.parent) return
    const d = e.data as FromHost | undefined
    if (d && typeof d === 'object' && typeof d.type === 'string' && d.type.startsWith('agentvc.')) fn(d)
  }
  window.addEventListener('message', on)
  return () => window.removeEventListener('message', on)
}

/**
 * Where a link from the branch graph goes: in the sidebar frame, the host's
 * review tab; everywhere else, this page. Picking a rebase target (`?pick=`)
 * stays in the frame because it only changes the graph.
 */
export function routeFor(path: string): 'host' | 'here' {
  return embedMode === 'sidebar' && !path.includes('pick=') ? 'host' : 'here'
}

const norm = (p: string) => p.replace(/[\\/]+$/, '')

/** The workspace this window is: the editor's folder when hosted, else the daemon's own. */
export function thisWorkspace(list: WorkspaceList): WorkspaceInfo | undefined {
  if (hostFolder) {
    const f = norm(hostFolder)
    // The deepest workspace holding the folder (worktrees can nest).
    const hit = list.workspaces
      .filter((w) => w.kind !== 'remote' && (norm(w.path) === f || f.startsWith(norm(w.path) + '/')))
      .sort((a, b) => b.path.length - a.path.length)[0]
    if (hit) return hit
  }
  return list.workspaces.find((w) => w.here) ?? list.workspaces.find((w) => w.id === list.here)
}
