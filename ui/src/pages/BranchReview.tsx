// One stacked branch of a workspace (`/review/b/:workspace/:branch`): what it
// changes, what you can do with it, and its conflicts when a restack stopped.

import { useState } from 'react'
import { useParams } from 'react-router'
import { api, ref } from '../api/client'
import { useLive } from '../api/live'
import type { ConflictHunk, HunkTake, PrBrief, WorkspaceInfo, WsConflicts, WsOp } from '../api/types'
import { useResource } from '../api/useResource'
import { ErrorNote, Skeleton } from '../components/bits'
import { HunkList } from '../components/ConflictResolver'
import { DiffView } from '../components/DiffView'
import { undoIn } from '../components/stackEdit'
import { useToast } from '../components/Toaster'
import { HELP, prWords, shortSha, WORDS } from '../core/words'
import { plural, timeAgo } from '../lib/format'

export function BranchReview() {
  const { workspace = '', '*': rest = '' } = useParams()
  const name = decodeURIComponent(rest)
  const toast = useToast()
  const { refresh } = useLive()
  const list = useResource('workspaces', api.workspaces)
  const w = list.data?.workspaces.find((x) => x.id === workspace)
  const b = w?.branches.find((x) => x.name === name)
  const detail = useResource(`branch:${workspace}:${name}:${b?.tip ?? ''}`, () => api.branch(workspace, name))
  const stopped = !!w && (w.pending !== null || w.branches.some((x) => x.conflicts > 0))
  const [busy, setBusy] = useState(false)
  const [open, setOpen] = useState<string | null>(null)

  const run = async (op: WsOp, done: string, keep = false) => {
    setBusy(true)
    try {
      const r = await api.stackEdit(workspace, op, keep)
      const stoppedAt = r.result?.conflict?.branch
      const mergeStop = r.result?.stopped
      if (mergeStop) {
        toast.ok(`Merged ${r.result?.merged?.length ?? 0}, then stopped at ${mergeStop.branch}: ${mergeStop.message}`, { undo: false })
        return
      }
      if (stoppedAt) toast.ok(`Restacking ${stoppedAt} stopped on a clash. Pick what each place should say below.`, { undo: false })
      else if (r.outcome === 'queued') toast.ok(`Queued: ${r.reason ?? 'an agent is working there'}. It applies when the agent stops.`, { undo: false })
      else toast.ok(done, { undo: op.op === 'checkout' || op.op === 'merge' ? false : undoIn(workspace, done) })
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }

  if (list.error) return <div className="page rv"><ErrorNote error={list.error} /></div>
  if (!list.data) return <div className="page rv"><Skeleton rows={4} /></div>
  if (!w || !b)
    return (
      <div className="page rv">
        <p className="muted">
          No branch <span className="mono">{name}</span> in that workspace any more.
        </p>
      </div>
    )

  const readOnly = w.kind === 'remote'
  // This branch and the stacked branches below it: what Merge lands.
  let below = 0
  for (let at: typeof b | undefined = b; at; at = w.branches.find((x) => x.name === at?.parent)) below++
  const d = detail.data
  return (
    <div className="page rv">
      <div className="rv-head">
        <div>
          <h1 className="rv-title mono">{b.name}</h1>
          <p className="muted">
            in {w.name}
            {w.kind === 'remote' ? ` on ${w.machine}` : ''} · on {b.parent.replace(/^origin\//, '')} · {plural(b.commits, 'commit')}
            {b.current ? ' · checked out' : ''}
          </p>
        </div>
        <span className={`tone tone-${b.conflicts > 0 || stopped ? 'conf' : b.needs_restack ? 'acc' : 'ok'}`}>
          {b.conflicts > 0 ? plural(b.conflicts, 'conflict') : b.needs_restack ? 'needs restack' : 'up to date'}
        </span>
      </div>

      {stopped && w.pending && <StackConflicts w={w} />}

      {!readOnly && (
        <div className="rv-actions rv-actions-list">
          {!b.current && (
            <div className="act">
              <button type="button" className="btn btn-primary" disabled={busy || stopped} onClick={() => void run({ op: 'checkout', branch: b.name }, `Checked out ${b.name} in ${w.name}.`)}>
                {WORDS.checkout}
              </button>
              <span className="act-help">{HELP.checkout}</span>
            </div>
          )}
          {b.needs_restack && !stopped && (
            <div className="act">
              <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void run({ op: 'restack', branch: b.name }, `Restacked ${b.name}.`, true)}>
                ↻ {WORDS.restack}
              </button>
              <span className="act-help">{HELP.restack} If something clashes, you resolve it right here.</span>
            </div>
          )}
          {b.pr && (
            <div className="act">
              <span className="bbadge bbadge-pr">PR #{b.pr.number}</span>
              <span className="act-help">{prLine(b.pr.state, b.pr.status)}</span>
            </div>
          )}
          {b.pr && (b.pr.state === 'open' || b.pr.state === null) && !stopped && (
            <div className="act">
              <button
                type="button"
                className="btn"
                disabled={busy || b.pr.status?.checks === 'fail' || b.pr.status?.review === 'changes_requested'}
                onClick={() => void run({ op: 'merge', branch: b.name }, `Merged up to ${b.name}.`)}
              >
                {WORDS.merge}
                {below > 1 ? ` ${below} PRs` : ''}
              </button>
              <span className="act-help">{HELP.merge(below)}</span>
            </div>
          )}
        </div>
      )}

      <section>
        <h2 className="rv-h2">Commits</h2>
        {detail.error && <ErrorNote error={detail.error} />}
        {!d && !detail.error && <Skeleton rows={2} />}
        {d && d.commits.length === 0 && <p className="muted">No commits of its own yet.</p>}
        {d && (
          <ul className="br-commits">
            {d.commits.map((c) => (
              <li key={c.commit}>
                <span className="mono muted">{shortSha(c.commit)}</span> {c.subject} <span className="muted">· {c.author} · {timeAgo(c.time)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      {d && d.files.length > 0 && (
        <section>
          <h2 className="rv-h2">
            Changes <span className="muted">· {plural(d.files.length, 'file')}</span>
          </h2>
          <div className="rv-files">
            {d.files.map((f) => (
              <div key={f.path} className="rv-file">
                <button type="button" className="rv-file-head" onClick={() => setOpen((o) => (o === f.path ? null : f.path))}>
                  <span className={`change change-${f.kind}`}>{f.kind}</span>
                  <span className="mono">{f.path}</span>
                  <span className="muted">{open === f.path ? '▾' : '▸'}</span>
                </button>
                {open === f.path && <DiffView base={ref.git(d.base)} head={ref.git(d.tip)} path={f.path} kind={f.kind} />}
              </div>
            ))}
          </div>
        </section>
      )}
    </div>
  )
}

/** The restack stopped in workspace `w`: resolve each place, then continue or abort. */
function StackConflicts({ w }: { w: WorkspaceInfo }) {
  const toast = useToast()
  const { refresh } = useLive()
  const data = useResource(`wsconf:${w.id}:${w.last_seen ?? 0}:${w.branches.map((b) => b.conflicts).join(',')}`, () => api.wsConflicts(w.id))
  const [busy, setBusy] = useState(false)
  const [latest, setLatest] = useState<WsConflicts | null>(null)
  const c = latest ?? data.data
  const open = c?.files ?? []

  const choose = async (path: string, h: ConflictHunk, take: HunkTake, custom?: string) => {
    try {
      const r = await api.wsResolve(w.id, path, [{ index: h.index, take, text: custom }])
      setLatest(await api.wsConflicts(w.id))
      toast.ok(r.open.length ? `${path}: resolved. ${plural(r.open.length, 'file')} left.` : 'All places resolved. Press Continue to finish the restack.', { undo: false })
    } catch (e) {
      toast.error(e)
    } finally {
      refresh()
    }
  }

  const finish = async (abort: boolean) => {
    setBusy(true)
    try {
      const r = abort ? await api.wsAbort(w.id) : await api.wsContinue(w.id)
      setLatest(r.conflicts)
      toast.ok(
        r.outcome === 'aborted'
          ? 'Restack stopped; the branches are as they were.'
          : r.outcome === 'stopped'
            ? `The next step clashes too: ${r.conflicts?.branch ?? ''}. Resolve it below.`
            : 'Restack finished.',
        { undo: false },
      )
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }

  return (
    <section className="conf-box" aria-label="Restack conflicts">
      <header>
        <b>
          Restacking {c?.branch ?? w.pending?.branch} onto {c?.parent ?? 'its parent'} stopped
        </b>
        <span className="muted">
          {c?.subject ? `at “${c.subject}”. ` : ''}
          {HELP.resolve}
        </span>
      </header>
      {data.error && <ErrorNote error={data.error} />}
      {!c && !data.error && <Skeleton rows={3} />}
      {c &&
        open.map((f) => (
          <HunkList key={f.path} path={f.path} hunks={f.hunks} mine={c.branch ?? 'mine'} onChoose={(h, take, text) => choose(f.path, h, take, text)} />
        ))}
      {c && (
        <footer className="conf-foot">
          <button type="button" className="btn btn-primary" disabled={busy || open.length > 0} onClick={() => void finish(false)}>
            Continue
          </button>
          <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => void finish(true)}>
            Abort restack
          </button>
          <span className="act-help">
            {open.length > 0 ? `${plural(open.length, 'file')} still to resolve.` : 'Commits the result and restacks the branches above.'} Abort puts every branch back as it was.
          </span>
        </footer>
      )}
    </section>
  )
}

/** What the PR needs, in a sentence, for the branch page. */
function prLine(state: string | null, st?: PrBrief | null): string {
  const w = prWords(state, st)
  if (!st) return w?.label ?? state ?? 'open'
  const parts = [w?.label ?? state ?? 'open']
  if (st.checks === 'fail' && st.failing?.length) parts.push(`failing: ${st.failing.join(', ')}`)
  if (st.unresolved > 0) parts.push(plural(st.unresolved, 'open thread'))
  parts.push(`checked ${timeAgo(st.at)}`)
  return parts.join(' · ')
}
