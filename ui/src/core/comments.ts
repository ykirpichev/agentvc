// Review comments (#168): where threads go in a diff, and loading them.

import { useEffect } from 'react'

import { api } from '../api/client'
import type { CommentSide, CommentTarget, CommentThread } from '../api/types'
import { useResource } from '../api/useResource'
import { actorName } from '../lib/format'

/** How often an open branch page with a PR pulls its GitHub review threads. */
const SYNC_EVERY_MS = 120_000

/** While a branch page with a GitHub PR is open: bring in its review
 * threads (and push replies made here) now and every two minutes, while the
 * tab is visible. Failures (offline, no token) stay quiet; threads that
 * could not be pushed say so themselves. */
export function useGithubSync(target: CommentTarget, pr: number | undefined, enabled: boolean) {
  useEffect(() => {
    if (pr === undefined || !enabled) return
    const run = () => {
      if (document.visibilityState === 'hidden') return
      api.syncComments(target).catch(() => {})
    }
    run()
    const t = window.setInterval(run, SYNC_EVERY_MS)
    return () => window.clearInterval(t)
  }, [target, pr, enabled])
}

/** What a diff needs to show and add comments. */
export interface DiffComments {
  target: CommentTarget
  threads: CommentThread[]
  /** Hosted viewers can read, not write. */
  readOnly?: boolean
}

/** The threads on `target` (your drafts included), refetched live. */
export function useThreads(target: CommentTarget | null) {
  const r = useResource(target ? `comments:${target}` : null, () => api.comments(target!))
  return { threads: r.data?.threads ?? [], error: r.error, loading: r.loading }
}

/** Threads of one file, by where they show: under a line, or above the file. */
export function placeThreads(threads: CommentThread[], path: string) {
  const at = new Map<string, CommentThread[]>()
  const loose: CommentThread[] = []
  for (const t of threads) {
    if (t.anchor?.path !== path) continue
    if (!t.current) {
      loose.push(t)
      continue
    }
    const k = lineKey(t.anchor.side, t.current.end)
    at.set(k, [...(at.get(k) ?? []), t])
  }
  return { at, loose }
}

export const lineKey = (side: CommentSide, line: number) => `${side}:${line}`

/** Who wrote it, for people: "you", the agent's id, the GitHub login. */
export function who(author: string): string {
  if (author.startsWith('github:')) return `@${actorName(author)}`
  return actorName(author) || author
}

/** Who started the thread. */
export const authorOf = (t: CommentThread) => t.comments[0]?.author ?? ''

/** Scroll to a thread once its file is open. */
export function scrollToThread(id: string) {
  let tries = 0
  const go = () => {
    const el = document.getElementById(`thread-${id}`)
    if (el) {
      el.scrollIntoView({ block: 'center', behavior: 'smooth' })
      el.classList.add('cthread-flash')
      window.setTimeout(() => el.classList.remove('cthread-flash'), 1200)
    } else if (tries++ < 20) window.setTimeout(go, 100)
  }
  go()
}

/** How many threads are open (waiting on the agent), addressed (waiting on
 * you to resolve) and drafts (not sent), and those words, so every count of
 * one target's threads says the same: "1 open · 2 addressed · 1 draft". */
export function threadCounts(threads: CommentThread[]) {
  const n = { open: 0, addressed: 0, drafts: 0 }
  for (const t of threads) {
    if (t.state === 'open') n.open++
    else if (t.state === 'addressed') n.addressed++
    else if (t.state === 'pending') n.drafts++
  }
  const parts = [
    n.open ? `${n.open} open` : '',
    n.addressed ? `${n.addressed} addressed` : '',
    n.drafts ? `${n.drafts} ${n.drafts === 1 ? 'draft' : 'drafts'}` : '',
  ].filter(Boolean)
  return { ...n, words: parts.length ? parts.join(' · ') : 'none open' }
}
