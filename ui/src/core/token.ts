// The daemon token (#199). agentvcd takes changes (anything but a read) only
// with its token in the `x-agentvc-token` header. The pages agentvcd serves
// carry it in `<meta name="agentvc-token">`. Under the Vite dev server, which
// serves its own page, the dev proxy adds it; a `?token=` in the address is
// also taken (kept for this origin and dropped from the address bar). The
// hosted service relays calls with the token itself, so it needs none here.

import { hosted } from './base'

const KEY = 'agentvc.token'
export const TOKEN_HEADER = 'x-agentvc-token'

/** Take `?token=` from `url` into `store`; the address without it, or null when there was none. */
export function adoptToken(url: string, store: Pick<Storage, 'setItem'> | null): string | null {
  const u = new URL(url)
  const t = u.searchParams.get('token')
  if (t === null) return null
  if (t.trim()) {
    try {
      store?.setItem(KEY, t.trim())
    } catch {
      // No storage: the token lasts for this page only.
    }
    remembered = t.trim()
  }
  u.searchParams.delete('token')
  return u.pathname + (u.search === '?' ? '' : u.search) + u.hash
}

let remembered: string | null = null

function storage(): Storage | null {
  try {
    return window.localStorage
  } catch {
    return null
  }
}

if (!hosted && typeof window !== 'undefined') {
  const clean = adoptToken(window.location.href, storage())
  if (clean !== null) window.history.replaceState(window.history.state, '', clean)
}

/** The token in the page's `<meta name="agentvc-token">`, if any. */
export function pageToken(doc: Pick<Document, 'querySelector'> | null): string | null {
  const t = doc?.querySelector('meta[name="agentvc-token"]')?.getAttribute('content')?.trim()
  return t || null
}

/** The token to send, if this browser has one. */
export function daemonToken(): string | null {
  if (hosted) return null
  const fromPage = pageToken(typeof document === 'undefined' ? null : document)
  if (fromPage) return fromPage
  if (remembered) return remembered
  try {
    return storage()?.getItem(KEY) ?? null
  } catch {
    return null
  }
}

/** Headers that authorize a change. */
export function tokenHeaders(): Record<string, string> {
  const t = daemonToken()
  return t ? { [TOKEN_HEADER]: t } : {}
}
