// Review comments (#168): where threads go in a diff, and loading them.

import { api } from '../api/client'
import type { CommentSide, CommentTarget, CommentThread } from '../api/types'
import { useResource } from '../api/useResource'
import { actorName } from '../lib/format'

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
