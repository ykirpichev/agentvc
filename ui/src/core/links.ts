// Where each thing in the branch graph lives, so every surface links the same way.

import { useLocation } from 'react-router'

export type Selection = { kind: 'version'; id: number } | { kind: 'task'; id: number } | { kind: 'commit'; commit: string } | null

export function selectionFromPath(path: string): Selection {
  let m = /^\/review\/v\/(\d+)/.exec(path)
  if (m) return { kind: 'version', id: Number(m[1]) }
  m = /^\/review\/t\/(\d+)/.exec(path)
  if (m) return { kind: 'task', id: Number(m[1]) }
  m = /^\/review\/c\/([0-9a-f]+)/.exec(path)
  if (m) return { kind: 'commit', commit: m[1]! }
  return null
}

export const versionLink = (id: number, onto?: string) => `/review/v/${id}${onto ? `?onto=${onto}` : ''}`
export const taskLink = (id: number) => `/review/t/${id}`
export const commitLink = (c: string) => `/review/c/${c}`

/** "Rebase onto…" picks a target by click; this is the version being moved. */
export function usePick() {
  const loc = useLocation()
  const pick = new URLSearchParams(loc.search).get('pick')
  return pick ? Number(pick) : null
}

