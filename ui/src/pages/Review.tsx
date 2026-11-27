// The review screen: the branch graph lives in the sidebar, and this pane
// shows whatever is selected in it. A task shows its next step and its
// versions side by side; a version shows its actions, a preview of a move,
// its conflicts and its changes; a main commit shows who is behind it.

import { useState } from 'react'
import { Link, useNavigate, useParams, useSearchParams } from 'react-router'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { Graph, GraphAttempt } from '../api/types'
import { useResource } from '../api/useResource'
import { Empty, ErrorNote, Skeleton } from '../components/bits'
import { commitLink, taskLink, versionLink } from '../core/links'
import { useReview } from './reviewContext'
import { ConflictResolver } from '../components/ConflictResolver'
import { DiffView } from '../components/DiffView'
import { MovePreview } from '../components/MovePreview'
import { useToast } from '../components/Toaster'
import { stackStale, visibleVersions } from '../core/graph'
import { nextStep, type NextStep } from '../core/next'
import { agentLine, HELP, shortSha, statusWords, testsWords, tryName, versionName, WORDS } from '../core/words'
import { money, plural, timeAgo } from '../lib/format'
import { CompareView } from './Compare'
import { WorkspacesOverview } from '../components/WorkspacesOverview'

const versionsOf = (graph: Graph, task: number) =>
  visibleVersions(graph, { showClosed: true }).filter((g) => g.attempt.task_id === task)

/** `/review`: nothing picked yet; agent work that waits on you, if any. */
export function ReviewHome() {
  const { graph, tasks } = useReview()
  if (!graph || !tasks) return <div className="page"><Skeleton rows={6} /></div>
  const rows = tasks
    .map((t) => ({ t, next: nextStep(versionsOf(graph, t.id)), n: versionsOf(graph, t.id).length }))
    .filter((r) => r.n > 0 && r.next.kind !== 'done')
  return (
    <div className="page rv">
      <WorkspacesOverview />
      {rows.length === 0 ? null : (
        <>
          <h1 className="rv-title">Agent work</h1>
          <div className="rv-list">
            {rows.map(({ t, next, n }) => (
              <Link key={t.id} to={taskLink(t.id)} className="rv-item">
                <span className="rv-item-name">{t.goal}</span>
                <span className="muted">
                  {n} {n === 1 ? 'try' : 'tries'} · {next.kind === 'wait' ? 'agents working' : next.label.toLowerCase()} · {timeAgo(t.updated_at)}
                </span>
              </Link>
            ))}
          </div>
        </>
      )}
    </div>
  )
}

/** `/review/t/:id` */
export function TaskReview() {
  const { id } = useParams()
  const taskId = Number(id)
  const { graph, tasks } = useReview()
  const navigate = useNavigate()
  const toast = useToast()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const task = tasks?.find((t) => t.id === taskId)
  const versions = graph ? versionsOf(graph, taskId) : []
  const open = versions.filter((g) => g.attempt.status !== 'abandoned')
  // Compare the two versions worth comparing: chosen and passing first.
  const rank = (g: GraphAttempt) => ({ promoted: 0, passed: 1, exploring: 2, failed: 3, abandoned: 4 })[g.attempt.status]
  const cmpIds = [...open].sort((a, b) => rank(a) - rank(b)).slice(0, 2).map((g) => g.attempt.id)
  const stats = useResource(Number.isFinite(taskId) ? `stats:${taskId}` : null, () => api.taskStats(taskId))
  const cmp = useResource(cmpIds.length >= 2 ? `compare:${cmpIds.join(',')}` : null, () => api.compare(cmpIds))

  if (!graph || !tasks) return <div className="page"><Skeleton rows={6} /></div>
  if (!task) return <div className="page"><Empty title="No such task">It may have been removed.</Empty></div>
  const next = nextStep(versions)

  const act = async (fn: () => Promise<unknown>, ok: string, undo = true) => {
    setBusy(true)
    try {
      await fn()
      toast.ok(ok, { undo })
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }

  const primary = () => {
    switch (next.kind) {
      case 'choose':
        document.getElementById('rv-versions')?.scrollIntoView({ behavior: 'smooth' })
        return
      case 'resolve':
        return navigate(versionLink(next.version))
      case 'update':
        return navigate(versionLink(next.version, graph.trunk[0]?.commit))
      case 'open-pr':
        return void act(() => api.openPr(next.version), 'PR opened', false)
      case 'done':
        window.open(next.url, '_blank', 'noopener')
        return
      case 'wait':
        return
    }
  }
  const primaryLabel: Record<NextStep['kind'], string | null> = {
    choose: null,
    wait: null,
    resolve: 'Resolve',
    update: WORDS.updateFromMain,
    'open-pr': WORDS.openPr,
    done: 'View PR ↗',
  }
  const statOf = (id: number) => stats.data?.find((s) => s.attempt === id)

  return (
    <div className="page rv">
      <div className="rv-head">
        <div>
          <h1 className="rv-title">{task.goal}</h1>
          <p className="muted">
            {versions.length} {versions.length === 1 ? 'try' : 'tries'}
            {task.criteria ? ` · done when ${task.criteria}` : ''}
          </p>
        </div>
        {primaryLabel[next.kind] && (
          <button type="button" className="btn btn-primary" disabled={busy} onClick={primary}>
            {primaryLabel[next.kind]}
          </button>
        )}
      </div>

      <div className="rv-tries" id="rv-versions">
        {versions.map((g) => {
          const a = g.attempt
          const s = statusWords(a.status)
          const st = statOf(a.id)
          const chosen = versions.some((v) => v.attempt.status === 'promoted')
          const summary = st?.summary ?? g.agent?.notes.at(-1)?.text ?? null
          return (
            <div key={a.id} className={`rv-try ${a.status === 'promoted' ? 'chosen' : ''} ${a.status === 'abandoned' ? 'closed' : ''}`}>
              <div className="rv-try-main">
                <Link to={versionLink(a.id)} className="rv-try-name">
                  {tryName(a, summary ?? a.prompt)}
                </Link>
                <span className="muted rv-try-meta">
                  {versionName(a)}
                  {st?.cost_usd != null ? ` · ${money(st.cost_usd)}` : ''}
                  {g.conflicts > 0 ? ` · ${`${g.conflicts} clash${g.conflicts === 1 ? '' : 'es'}`}` : ''} · {timeAgo(st?.last_activity ?? a.created_at)}
                </span>
              </div>
              <span className={`tone tone-${s.tone}`}>{g.tests && a.status === 'promoted' ? testsWords(g.tests).label : s.label}</span>
              {!chosen && (a.status === 'passed' || a.status === 'exploring' || a.status === 'failed') ? (
                <button
                  type="button"
                  className={`btn ${a.status === 'passed' ? 'btn-primary' : 'btn-ghost'}`}
                  disabled={busy}
                  title={HELP.choose}
                  onClick={() => void act(() => api.promote(a.id), `Using ${tryName(a, summary)}. The other tries stay in history.`)}
                >
                  {WORDS.choose}
                </button>
              ) : (
                <span />
              )}
            </div>
          )
        })}
      </div>

      {cmpIds.length >= 2 && (
        <section className="rv-section">
          <h2 className="rv-h2">Side by side</h2>
          {cmp.error && <ErrorNote error={cmp.error} />}
          {cmp.data ? <CompareView key={cmpIds.join(',')} data={cmp.data} embedded /> : <Skeleton rows={6} />}
        </section>
      )}
    </div>
  )
}

/** `/review/v/:id[?onto=<commit>]` */
export function VersionReview() {
  const { id } = useParams()
  const vid = Number(id)
  const [params] = useSearchParams()
  const onto = params.get('onto')
  const navigate = useNavigate()
  const { graph, tasks } = useReview()
  const toast = useToast()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const [discarding, setDiscarding] = useState(false)
  const [reason, setReason] = useState('')
  const diff = useResource(Number.isFinite(vid) ? `vdiff:${vid}` : null, () => api.diff(`a${vid}`))
  const [openFiles, setOpenFiles] = useState<Set<string>>(new Set())

  const g = graph?.attempts.find((x) => x.attempt.id === vid)
  if (!graph || !tasks) return <div className="page"><Skeleton rows={6} /></div>
  if (!g) return <div className="page"><Empty title="No such version">It may have been removed.</Empty></div>
  const a = g.attempt
  const task = tasks.find((t) => t.id === a.task_id)
  const siblings = a.task_id === null ? [] : versionsOf(graph, a.task_id)
  const chosen = siblings.find((s) => s.attempt.status === 'promoted')
  const isOpen = a.status === 'exploring' || a.status === 'passed' || a.status === 'failed'
  const base = graph.trunk.find((c) => c.commit === g.base_commit)
  const below = g.stacked_on ? graph.attempts.find((x) => x.attempt.id === g.stacked_on!.attempt) : undefined
  const stale = stackStale(graph, g)
  const behind = g.stacked_on ? 0 : (g.behind ?? 0)
  const s = statusWords(a.status)
  // Rebasing a chosen version with conflicts reopens it; its siblings still
  // say they lost to it, so choose it again once the conflicts are resolved.
  const wasChosen =
    a.status === 'exploring' &&
    !chosen &&
    siblings.some((x) => x.attempt.id !== a.id && x.attempt.reason === `lost to a${a.id}`)

  const act = async (fn: () => Promise<unknown>, ok: string, undo = true) => {
    setBusy(true)
    try {
      await fn()
      toast.ok(ok, { undo })
      return true
    } catch (e) {
      toast.error(e)
      return false
    } finally {
      setBusy(false)
      refresh()
    }
  }

  return (
    <div className="page rv">
      <div className="rv-head">
        <div>
          {task && <Link to={taskLink(task.id)} className="back">← {task.goal}</Link>}
          <h1 className="rv-title">
            {versionName(a)} <span className="muted mono rv-id" title="agentvc attempt id">a{a.id}</span>
          </h1>
          <p className="muted">
            {below ? (
              <>
                stacked on <Link to={versionLink(below.attempt.id)}>{versionName(below.attempt)}</Link>
                {stale ? ` · ${versionName(below.attempt)} changed since` : ' · up to date with it'}
              </>
            ) : (
              <>
                {base ? <>on <Link to={commitLink(base.commit)}>{base.summary}</Link> ({shortSha(base.commit)})</> : g.base_commit ? <>on {shortSha(g.base_commit)}</> : 'no base commit'}
                {behind > 0 ? ` · main has ${plural(behind, 'new commit')}` : base ? ' · up to date with main' : ''}
              </>
            )}
          </p>
        </div>
        <span className={`tone tone-${g.conflicts > 0 ? 'conf' : s.tone}`}>{g.conflicts > 0 ? plural(g.conflicts, 'conflict') : s.label}</span>
      </div>

      {a.prompt && <p className="rv-prompt">{a.prompt}</p>}

      <VersionFacts g={g} />

      <div className="rv-actions rv-actions-list">
        {isOpen && !chosen && g.conflicts === 0 && (
          <div className="act">
            <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void act(() => api.promote(a.id), `Chose ${versionName(a)}. The others stay in history.`)}>
              {WORDS.choose}
            </button>
            <span className="act-help">{HELP.choose}</span>
          </div>
        )}
        {(isOpen || a.status === 'promoted') && behind > 0 && g.conflicts === 0 && (
          <div className="act">
            <button type="button" className={`btn ${a.status === 'promoted' ? 'btn-primary' : 'btn-ghost'}`} disabled={busy || !graph.trunk[0]} onClick={() => navigate(versionLink(a.id, graph.trunk[0]!.commit))}>
              ↻ {WORDS.updateFromMain}
            </button>
            <span className="act-help">{HELP.updateFromMain(behind)}</span>
          </div>
        )}
        {(isOpen || a.status === 'promoted') && stale && g.conflicts === 0 && (
          <div className="act">
            <button type="button" className="btn btn-primary" disabled={busy} onClick={() => navigate(versionLink(a.id, `a${stale.attempt.id}`))}>
              ↻ {WORDS.updateFromParent(versionName(stale.attempt))}
            </button>
            <span className="act-help">{HELP.updateFromParent(versionName(stale.attempt))}</span>
          </div>
        )}
        {a.status === 'promoted' && behind === 0 && g.conflicts === 0 && (
          <div className="act">
            <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void act(() => api.openPr(a.id), g.pr ? 'PR updated' : 'PR opened', false)}>
              {g.pr ? 'Update PR' : WORDS.openPr}
            </button>
            <span className="act-help">{g.pr ? HELP.updatePr : below ? HELP.openPrStacked(versionName(below.attempt)) : HELP.openPr}</span>
          </div>
        )}
        {g.pr && (
          <div className="act">
            <a className="btn btn-ghost" href={g.pr.url} target="_blank" rel="noreferrer">
              PR #{g.pr.number} ↗
            </a>
            <span className="act-help">Opens the pull request on GitHub.</span>
          </div>
        )}
        {chosen && chosen.attempt.id !== a.id && isOpen && (
          <div className="act">
            <button
              type="button"
              className="btn btn-ghost"
              disabled={busy}
              onClick={() => void act(() => api.take(chosen.attempt.id, { source: `a${a.id}`, keep_conflicts: true }), `Added ${versionName(a)}'s changes to ${versionName(chosen.attempt)}`)}
            >
              Add its changes to {versionName(chosen.attempt)}
            </button>
            <span className="act-help">{HELP.take(versionName(chosen.attempt))}</span>
          </div>
        )}
        {isOpen && !discarding && (
          <div className="act">
            <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => setDiscarding(true)}>
              {WORDS.discard}
            </button>
            <span className="act-help">{HELP.discard}</span>
          </div>
        )}
      </div>

      {discarding && (
        <form
          className="rv-discard"
          onSubmit={(e) => {
            e.preventDefault()
            void act(() => api.abandon(a.id, reason.trim() || 'Discarded in review'), `Discarded ${versionName(a)}`).then(() => setDiscarding(false))
          }}
        >
          <input id="discard-reason" autoFocus placeholder="Why? (kept in history)" value={reason} onChange={(e) => setReason(e.target.value)} />
          <button type="submit" className="btn btn-primary" disabled={busy}>{WORDS.discard}</button>
          <button type="button" className="btn btn-ghost" onClick={() => setDiscarding(false)}>Cancel</button>
        </form>
      )}

      {onto && (
        <MovePreview
          key={`${a.id}:${onto}`}
          version={g}
          onto={onto}
          target={graph.trunk.find((c) => c.commit === onto)}
          targetVersion={/^a\d+$/.test(onto) ? graph.attempts.find((x) => `a${x.attempt.id}` === onto) : undefined}
          onDone={() => navigate(versionLink(a.id), { replace: true })}
          onCancel={() => navigate(versionLink(a.id), { replace: true })}
        />
      )}

      {g.conflicts > 0 && <ConflictResolver key={`c${a.id}`} version={g} rechoose={wasChosen} />}

      <section className="rv-section">
        <h2 className="rv-h2">Changes {diff.data && <span className="muted">· {plural(diff.data.changes.length, 'file')}</span>}</h2>
        {diff.error && <ErrorNote error={diff.error} />}
        {!diff.data && !diff.error && <Skeleton rows={4} />}
        {diff.data && diff.data.changes.length === 0 && <p className="muted">No changes yet.</p>}
        {diff.data && (
          <div className="rv-files">
            {diff.data.changes.map((c) => {
              const open = openFiles.has(c.path)
              return (
                <div key={c.path} className="rv-file">
                  <button
                    type="button"
                    className="rv-file-head"
                    aria-expanded={open}
                    onClick={() => setOpenFiles((s) => { const n = new Set(s); if (n.has(c.path)) n.delete(c.path); else n.add(c.path); return n })}
                  >
                    <span className={`change change-${c.kind}`}>{c.kind}</span>
                    <span className="mono">{c.path}</span>
                    <span className="muted">{open ? '▾' : '▸'}</span>
                  </button>
                  {open && <DiffView base={diff.data!.a_ref} head={diff.data!.b_ref} path={c.path} kind={c.kind} />}
                </div>
              )
            })}
          </div>
        )}
      </section>
    </div>
  )
}

/** Tests and agent metadata (cost, tokens, notes, transcript), only when something was recorded. */
function VersionFacts({ g }: { g: GraphAttempt }) {
  const t = g.tests ? testsWords(g.tests) : null
  const line = agentLine(g.agent, g.attempt.model)
  const notes = g.agent?.notes ?? []
  const transcripts = g.agent?.transcripts ?? []
  if (!t && !line && !notes.length && !transcripts.length) return null
  return (
    <div className="rv-facts">
      <p className="rv-facts-line">
        {t && (
          <span className={`tone tone-${t.tone}`} title={t.help}>
            {t.label}
          </span>
        )}
        {t && !g.tests!.stale && g.tests!.summary && (
          <span className="muted">{g.tests!.summary.replace(/^(pass|fail) · /, '')}</span>
        )}
        {line && <span className="muted">{line}</span>}
        {transcripts.map((tr) =>
          /^https?:\/\//.test(tr) ? (
            <a key={tr} href={tr} target="_blank" rel="noreferrer">
              Transcript ↗
            </a>
          ) : (
            <span key={tr} className="muted mono rv-facts-path" title={tr}>
              transcript {tr.split('/').pop()}
            </span>
          ),
        )}
      </p>
      {notes.map((n, i) => (
        <p key={i} className="rv-note">
          {n.text} <span className="muted">· {n.actor.replace(/^[a-z]+:/, '')}, {timeAgo(n.created_at)}</span>
        </p>
      ))}
    </div>
  )
}

/** `/review/c/:sha` */
export function CommitReview() {
  const { sha } = useParams()
  const { graph } = useReview()
  const navigate = useNavigate()
  if (!graph) return <div className="page"><Skeleton rows={4} /></div>
  const c = graph.trunk.find((t) => t.commit.startsWith(sha ?? '-'))
  if (!c) return <div className="page"><Empty title="Commit not in the graph" /></div>
  const head = graph.trunk[0]?.commit === c.commit
  const older = head
    ? visibleVersions(graph).filter((g) => (g.behind ?? 0) > 0 && g.conflicts === 0 && g.attempt.status !== 'abandoned')
    : []
  return (
    <div className="page rv">
      <h1 className="rv-title mono">{c.summary}</h1>
      <p className="muted">
        {head ? `${graph.upstream} · ` : ''}
        {c.on_upstream ? '' : 'not on main · '}
        {c.commit} · {timeAgo(c.time)}
      </p>
      {older.length > 0 && (
        <section className="rv-section">
          <p>{plural(older.length, 'version')} started before this commit. Drag one here in the graph, or update it:</p>
          <div className="rv-actions">
            {older.map((g) => (
              <button key={g.attempt.id} type="button" className="btn btn-ghost" onClick={() => navigate(versionLink(g.attempt.id, c.commit))}>
                ↻ {versionName(g.attempt)} <span className="muted">a{g.attempt.id}</span>
              </button>
            ))}
          </div>
        </section>
      )}
    </div>
  )
}
