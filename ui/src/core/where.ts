// Where a workspace lives: its folder and machine, in words people read.

import type { WorkspaceInfo, WorkspaceList } from '../api/types'

/** `~/sources/agentvc-2`: the folder, home shortened, as the daemon says it. */
export function folderOf(w: WorkspaceInfo): string {
  return w.folder ?? w.path
}

/** A folder cut to its last parts so the end (the part that differs) shows: `…/mono/win-a`. */
export function shortFolder(f: string, max = 30): string {
  if (f.length <= max) return f
  const parts = f.split('/')
  let out = parts.pop() ?? f
  while (parts.length && out.length + parts[parts.length - 1]!.length + 3 <= max) out = `${parts.pop()}/${out}`
  return `…/${out}`
}

/** The machines a list of workspaces is spread over, this one first. */
export function machinesOf(list: WorkspaceList): string[] {
  const out: string[] = []
  const first = list.machine ?? list.workspaces.find((w) => w.kind !== 'remote')?.machine
  if (first && list.workspaces.some((w) => w.machine === first)) out.push(first)
  for (const w of list.workspaces) if (!out.includes(w.machine)) out.push(w.machine)
  return out
}

/** The separate checkout folders (clones) a list covers; worktrees share their clone's. */
export function checkoutsOf(list: WorkspaceList): string[] {
  return [...new Set(list.workspaces.map((w) => `${w.machine}:${w.checkout}`))]
}

/** `worktree of ~/src/mono`, `clone`, `published from mac`: how it relates to its checkout. */
export function kindWords(w: WorkspaceInfo): string {
  if (w.kind === 'remote') return `published from ${w.machine}`
  if (w.kind === 'worktree') return `worktree of ${base(w.checkout)}`
  return 'checkout folder'
}

const base = (p: string) => p.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || p

/** Hover text: everything about where a workspace is. */
export function whereTitle(w: WorkspaceInfo): string {
  return [
    `${w.name} (${w.id})`,
    `Folder: ${w.path}`,
    `Machine: ${w.machine}${w.this_machine === false ? '' : ' (this machine)'}`,
    `Kind: ${w.kind === 'worktree' ? `worktree of ${w.checkout}` : kindWords(w)}`,
    w.user ? `Created by: ${w.user}` : '',
  ]
    .filter(Boolean)
    .join('\n')
}
