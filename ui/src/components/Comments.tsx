// Review comments on diffs (#168): threads under their lines, the box to
// write one, the review bar that sends drafts, and the list of threads.
// The same code runs in the web UI, the VS Code/Cursor panel and the Claude
// Code pane, since all of them show the daemon's own pages.

import { useEffect, useRef, useState, type KeyboardEvent } from 'react'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { Agent, CommentTarget, CommentThread } from '../api/types'
import { canFollowUp } from '../core/agents'
import { authorOf, who } from '../core/comments'
import { plural, timeAgo } from '../lib/format'
import { Icon } from './Icon'
import { useToast } from './Toaster'
import './comments.css'

const STATE_WORD: Record<CommentThread['state'], string> = {
  pending: 'draft',
  open: 'open',
  addressed: 'addressed',
  resolved: 'resolved',
}

/** Write a comment: Cmd/Ctrl+Enter adds it to your review. */
export function CommentBox({
  label,
  onSave,
  onCancel,
  allowNow = true,
  placeholder = 'Leave a comment',
}: {
  label: string
  onSave: (body: string, now: boolean) => Promise<void>
  onCancel: () => void
  allowNow?: boolean
  placeholder?: string
}) {
  const [body, setBody] = useState('')
  const [busy, setBusy] = useState(false)
  const ref = useRef<HTMLTextAreaElement>(null)
  useEffect(() => ref.current?.focus(), [])
  const save = async (now: boolean) => {
    if (!body.trim() || busy) return
    setBusy(true)
    try {
      await onSave(body, now)
      setBody('')
    } finally {
      setBusy(false)
    }
  }
  const keys = (e: KeyboardEvent) => {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault()
      void save(false)
    } else if (e.key === 'Escape') {
      e.preventDefault()
      onCancel()
    }
  }
  return (
    <div className="cbox">
      <div className="cbox-label muted">{label}</div>
      <textarea
        ref={ref}
        className="input cbox-text"
        rows={3}
        value={body}
        placeholder={placeholder}
        aria-label={label}
        onChange={(e) => setBody(e.target.value)}
        onKeyDown={keys}
      />
      <div className="cbox-acts">
        <button type="button" className="btn btn-primary" disabled={busy || !body.trim()} onClick={() => void save(false)} title="Cmd/Ctrl+Enter">
          Add to review
        </button>
        {allowNow && (
          <button type="button" className="btn" disabled={busy || !body.trim()} onClick={() => void save(true)} title="Post it on its own, without waiting for the rest of the review">
            Comment now
          </button>
        )}
        <button type="button" className="btn btn-ghost" disabled={busy} onClick={onCancel}>
          Cancel
        </button>
      </div>
    </div>
  )
}

/** One thread with its replies and actions. */
export function ThreadView({ t, readOnly, showWhere = false }: { t: CommentThread; readOnly?: boolean; showWhere?: boolean }) {
  const toast = useToast()
  const { refresh } = useLive()
  const [replying, setReplying] = useState(false)
  const [busy, setBusy] = useState(false)
  const done = t.state === 'resolved'
  const [folded, setFolded] = useState(done)
  const act = async (f: () => Promise<unknown>, msg?: string) => {
    setBusy(true)
    try {
      await f()
      if (msg) toast.ok(msg, { undo: false })
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }
  const a = t.anchor
  const where = a
    ? `${a.path}:${t.current ? (t.current.start === t.current.end ? t.current.start : `${t.current.start}-${t.current.end}`) : a.start}${a.side === 'old' ? ' (old)' : ''}`
    : 'whole change'
  return (
    <div className={`cthread cthread-${t.state} ${t.outdated ? 'cthread-outdated' : ''}`} id={`thread-${t.id}`} data-thread={t.id}>
      <header className="cthread-head">
        <button type="button" className="cthread-fold" aria-expanded={!folded} onClick={() => setFolded((f) => !f)} title={folded ? 'Show' : 'Hide'}>
          <Icon name="chevron" size={12} className={folded ? '' : 'rot90'} />
        </button>
        <span className={`cstate cstate-${t.state}`}>{STATE_WORD[t.state]}</span>
        {t.outdated && <span className="cstate cstate-outdated" title="The lines it was written on are gone from the file">outdated</span>}
        {t.github &&
          (t.github.url ? (
            <a className="cstate cstate-gh" href={t.github.url} target="_blank" rel="noreferrer" title={t.source === 'github' ? 'Written on GitHub' : `On PR #${t.github.pr}`}>
              GitHub
            </a>
          ) : (
            <span className="cstate cstate-gh">GitHub</span>
          ))}
        {t.github?.error && (
          <span className="cthread-gherr" title={t.github.error}>
            not on GitHub yet
          </span>
        )}
        {(showWhere || t.outdated) && <span className="mono cthread-where">{where}</span>}
        <span className="cthread-meta muted">
          {folded ? `${who(authorOf(t))} · ` : ''}
          {t.comments.length > 1 ? plural(t.comments.length - 1, 'reply') : timeAgo(t.created_at)}
        </span>
      </header>
      {!folded && (
        <>
          {t.outdated && a && (
            <pre className="cquote" aria-label="The lines it was written on">
              {a.lines.join('\n')}
            </pre>
          )}
          <ol className="ccomments">
            {t.comments.map((c) => (
              <li key={c.id} className={`ccomment ${c.author.startsWith('agent:') ? 'ccomment-agent' : ''}`}>
                <span className="ccomment-who">{who(c.author)}</span>
                <span className="muted"> · {timeAgo(c.at)}</span>
                <div className="ccomment-body">{c.body}</div>
              </li>
            ))}
          </ol>
          {!readOnly && (
            <div className="cthread-acts">
              {t.state === 'pending' ? (
                <button type="button" className="btn btn-ghost btn-sm" disabled={busy} onClick={() => void act(() => api.deleteDraft(t.id))}>
                  Delete draft
                </button>
              ) : (
                <>
                  {!replying && (
                    <button type="button" className="btn btn-sm" disabled={busy} onClick={() => setReplying(true)}>
                      Reply
                    </button>
                  )}
                  {done ? (
                    <button type="button" className="btn btn-ghost btn-sm" disabled={busy} onClick={() => void act(() => api.reopenComment(t.id))}>
                      Reopen
                    </button>
                  ) : (
                    <button type="button" className="btn btn-ghost btn-sm" disabled={busy} onClick={() => void act(() => api.resolveComment(t.id))}>
                      <Icon name="check" size={13} /> Resolve
                    </button>
                  )}
                </>
              )}
            </div>
          )}
          {replying && (
            <CommentBox
              label="Reply"
              placeholder="Reply"
              allowNow={false}
              onCancel={() => setReplying(false)}
              onSave={async (body) => {
                await act(() => api.replyComment(t.id, body))
                setReplying(false)
              }}
            />
          )}
        </>
      )}
    </div>
  )
}

/** Your drafts on a target: send them as one review, to its agent if one works on it. */
export function ReviewBar({
  target,
  threads,
  agent,
  readOnly,
  pr,
}: {
  target: CommentTarget
  threads: CommentThread[]
  agent?: Agent | null
  readOnly?: boolean
  /** The branch's open GitHub PR: offer to post the review there too. */
  pr?: number
}) {
  const toast = useToast()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const [toGithub, setToGithub] = useState(false)
  const drafts = threads.filter((t) => t.state === 'pending')
  const open = threads.filter((t) => t.state === 'open' || t.state === 'addressed')
  if (readOnly || (drafts.length === 0 && open.length === 0)) return null
  const send = async (toAgent: boolean) => {
    setBusy(true)
    try {
      const gh = toGithub && pr !== undefined
      const r = await api.submitReview(target, toAgent ? (gh ? 'both' : 'agent') : gh ? 'github' : 'none')
      const n = plural(r.threads.length, 'comment')
      const onPr = r.review.github_review ? ` Also posted as a review of PR #${pr}.` : ''
      toast.ok(
        (r.delivery === 'prompt'
          ? `Sent ${n} to ${r.agent?.id ?? 'the agent'}; it started working on them.`
          : r.delivery === 'queued'
            ? `Sent ${n}. ${r.agent?.id ?? 'The agent'} is working and gets them at its next step.`
            : r.delivery === 'waiting'
              ? `Sent ${n}. No agent works on this yet; the next one gets them.`
              : `Posted ${n}.`) + onPr,
        { undo: false },
      )
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }
  const discard = async () => {
    setBusy(true)
    try {
      const r = await api.discardDrafts(target)
      toast.ok(`Discarded ${plural(r.discarded.length, 'draft')}.`, { undo: false })
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }
  const canAgent = !!agent && agent.state !== 'discarded' && (agent.state !== 'landed' || canFollowUp(agent))
  return (
    <div className={`creview ${drafts.length ? 'creview-drafts' : ''}`} role="region" aria-label="Review">
      <Icon name="comment" />
      {drafts.length > 0 ? (
        <>
          <b>{plural(drafts.length, 'comment')} not sent</b>
          <span className="creview-acts">
            {pr !== undefined && (
              <label className="creview-gh" title={`Post them as one review of PR #${pr} too: line comments where the lines are in its diff, the rest in the review's text`}>
                <input type="checkbox" checked={toGithub} onChange={(e) => setToGithub(e.target.checked)} disabled={busy} /> Also on PR #{pr}
              </label>
            )}
            {canAgent && (
              <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void send(true)} title={`${agent!.title} (${agent!.id}) gets them as its next prompt, or at its next step while it works`}>
                Send to {agent!.id}
              </button>
            )}
            <button type="button" className={`btn ${canAgent ? 'btn-ghost' : 'btn-primary'}`} disabled={busy} onClick={() => void send(false)} title="Publish them here; an agent working on this branch later gets them too">
              {canAgent ? 'Just post' : 'Post comments'}
            </button>
            <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => void discard()}>
              Discard
            </button>
          </span>
        </>
      ) : (
        <span>
          {plural(open.filter((t) => t.state === 'open').length, 'open comment')}
          {open.some((t) => t.state === 'addressed') ? ` · ${open.filter((t) => t.state === 'addressed').length} addressed by the agent, waiting for you to resolve` : ''}
        </span>
      )}
    </div>
  )
}

/** All threads on a target, open first; click one to jump to it. `n`/`p`
 * step through them. Comments on the whole change show here in full, with a
 * box to write one. */
export function ThreadList({
  target,
  threads,
  onJump,
  readOnly,
}: {
  target: CommentTarget
  threads: CommentThread[]
  onJump: (t: CommentThread) => void
  readOnly?: boolean
}) {
  const toast = useToast()
  const { refresh } = useLive()
  const [showDone, setShowDone] = useState(false)
  const [writing, setWriting] = useState(false)
  useThreadKeys(threads, onJump)
  if (threads.length === 0 && readOnly) return null
  const rank = (t: CommentThread) => (t.state === 'pending' ? 0 : t.state === 'open' ? 1 : t.state === 'addressed' ? 2 : 3)
  const sorted = [...threads].sort((a, b) => rank(a) - rank(b) || a.created_at - b.created_at)
  const done = sorted.filter((t) => t.state === 'resolved')
  const shown = (showDone ? sorted : sorted.filter((t) => t.state !== 'resolved')).filter((t) => t.anchor)
  const general = (showDone ? sorted : sorted.filter((t) => t.state !== 'resolved')).filter((t) => !t.anchor)
  const save = async (body: string, now: boolean) => {
    try {
      await api.addComment({ target, body, pending: !now })
      setWriting(false)
    } catch (e) {
      toast.error(e)
      throw e
    } finally {
      refresh()
    }
  }
  return (
    <section className="clist" aria-label="Comments">
      <h2 className="rv-h2">
        Comments <span className="muted">· {threads.length - done.length} open</span>
        {threads.length > 0 && <span className="muted clist-keys"> · n / p to step through</span>}
        {!readOnly && !writing && (
          <button type="button" className="btn btn-ghost btn-sm clist-add" onClick={() => setWriting(true)} title="A comment on the whole change, not on a line. To comment on lines, click a line number in a diff.">
            <Icon name="plus" size={12} /> Comment
          </button>
        )}
      </h2>
      {threads.length === 0 && !writing && <p className="muted clist-hint">Click a line number in a diff below to comment on it (drag for several lines).</p>}
      {shown.length > 0 && (
        <ul>
          {shown.map((t) => (
            <li key={t.id}>
              <button type="button" className={`clist-row cthread-${t.state}`} onClick={() => onJump(t)}>
                <span className={`cstate cstate-${t.state}`}>{STATE_WORD[t.state]}</span>
                {t.outdated && <span className="cstate cstate-outdated">outdated</span>}
                <span className="mono clist-where">{t.anchor ? `${t.anchor.path}:${t.current?.start ?? t.anchor.start}` : 'whole change'}</span>
                <span className="clist-body">{t.comments[0]?.body}</span>
                <span className="muted">{who(authorOf(t))}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
      {general.map((t) => (
        <ThreadView key={t.id} t={t} readOnly={readOnly} />
      ))}
      {writing && <CommentBox label="On the whole change" onSave={save} onCancel={() => setWriting(false)} />}
      {done.length > 0 && (
        <button type="button" className="btn btn-ghost btn-sm" onClick={() => setShowDone((s) => !s)}>
          {showDone ? 'Hide resolved' : `Show ${done.length} resolved`}
        </button>
      )}
    </section>
  )
}

/** `n` / `p`: the next / previous thread, when not typing. */
function useThreadKeys(threads: CommentThread[], onJump: (t: CommentThread) => void) {
  const at = useRef(-1)
  useEffect(() => {
    const open = threads.filter((t) => t.state !== 'resolved')
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey || (e.key !== 'n' && e.key !== 'p')) return
      const el = e.target as HTMLElement | null
      if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable)) return
      if (open.length === 0) return
      e.preventDefault()
      at.current = e.key === 'n' ? (at.current + 1) % open.length : (at.current - 1 + open.length) % open.length
      const t = open[at.current]
      if (t) onJump(t)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [threads, onJump])
}
