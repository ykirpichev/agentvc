// Where each thing in the branch graph lives, so every surface links the same way.

import { useLocation, useNavigate } from 'react-router'
import { routeFor, toHost } from './embed'

export type Selection =
  | { kind: 'version'; id: number }
  | { kind: 'task'; id: number }
  | { kind: 'commit'; commit: string }
  | { kind: 'branch'; workspace: string; name: string }
  | { kind: 'agent'; id: string }
  | null

export function selectionFromPath(path: string): Selection {
  let m = /^\/review\/v\/(\d+)/.exec(path)
  if (m) return { kind: 'version', id: Number(m[1]) }
  m = /^\/review\/t\/(\d+)/.exec(path)
  if (m) return { kind: 'task', id: Number(m[1]) }
  m = /^\/review\/c\/([0-9a-f]+)/.exec(path)
  if (m) return { kind: 'commit', commit: m[1]! }
  m = /^\/review\/a\/([^/]+)/.exec(path)
  if (m) return { kind: 'agent', id: decodeURIComponent(m[1]!) }
  m = /^\/review\/b\/([^/]+)\/(.+)$/.exec(path)
  if (m) return { kind: 'branch', workspace: decodeURIComponent(m[1]!), name: decodeURIComponent(m[2]!) }
  return null
}

export const versionLink = (id: number, onto?: string) => `/review/v/${id}${onto ? `?onto=${onto}` : ''}`
export const taskLink = (id: number) => `/review/t/${id}`
export const commitLink = (c: string) => `/review/c/${c}`
/** A stacked branch of a workspace (`?resolve=1` opens its stopped restack). */
export const branchLink = (workspace: string, name: string) =>
  `/review/b/${encodeURIComponent(workspace)}/${name.split('/').map(encodeURIComponent).join('/')}`

/**
 * The Code tab (#172) at a ref (`branch:w1/feat`, `agent:ag3`, `workspace:w1`,
 * `main`, `aN`, `git:<sha>`) and path, optionally at lines (`#L10-L20`).
 */
export const codeLink = (r: string, path = '', lines?: [number, number?]) =>
  `/review/code/${encodeURIComponent(r)}${path ? `/${path.split('/').map(encodeURIComponent).join('/')}` : ''}${
    lines ? `#L${lines[0]}${lines[1] && lines[1] !== lines[0] ? `-L${lines[1]}` : ''}` : ''
  }`

/** "Rebase onto…" picks a target by click; this is the version being moved. */
export function usePick() {
  const loc = useLocation()
  const pick = new URLSearchParams(loc.search).get('pick')
  return pick ? Number(pick) : null
}


/**
 * Navigate from the branch graph. Inside an editor's sidebar the host also
 * opens the path in its review tab; the frame follows so the selection shows.
 */
export function useGo() {
  const navigate = useNavigate()
  return (path: string) => {
    if (routeFor(path) === 'host') toHost({ type: 'agentvc.open', path })
    navigate(path)
  }
}
