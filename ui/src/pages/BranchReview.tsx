// One stacked branch of a workspace (`/review/b/:workspace/:branch`): what it
// changes, what you can do with it, and its conflicts when a restack stopped.

import { useState, type FormEvent } from 'react'
import { Link, useParams } from 'react-router'
import { api, ref } from '../api/client'
import { useLive } from '../api/live'
import type { ConflictHunk, HunkTake, PrBrief, WorkspaceInfo, WsConflicts, WsOp } from '../api/types'
import { useResource } from '../api/useResource'
import { ErrorNote, Skeleton } from '../components/bits'
import { Icon } from '../components/Icon'
import { HunkList } from '../components/ConflictResolver'
import { DiffView } from '../components/DiffView'
import { ReviewBar, ThreadList } from '../components/Comments'
import { scrollToThread, useGithubSync, useThreads } from '../core/comments'
import { undoIn } from '../components/stackEdit'
import { useToast } from '../components/Toaster'
import { canMoveBranch } from '../core/graph'
import { HELP, prWords, shortSha, WORDS } from '../core/words'
import { plural, timeAgo } from '../lib/format'
import { agentLink, agentsOn, agentState, agentWorkingOn, resolverOf, resolvingWords, settingsLine } from '../core/agents'
import { AgentSettingsForm, AgentsFrom, StartAgent } from './Agents'
import { branchLink, codeLink, useGo } from '../core/links'
import { useReview } from './reviewContext'

export function BranchReview() {
  const { workspace = '', '*': rest = '' } = useParams()
  const name = decodeURIComponent(rest)
  const toast = useToast()
  const go = useGo()
  const { refresh } = useLive()
  const list = useResource('workspaces', api.workspaces)
  const w = list.data?.workspaces.find((x) => x.id === workspace)
  const b = w?.branches.find((x) => x.name === name)
  const detail = useResource(`branch:${workspace}:${name}:${b?.tip ?? ''}`, () => api.branch(workspace, name))
  const stopped = !!w && (w.pending !== null || w.branches.some((x) => x.conflicts > 0))
  const [busy, setBusy] = useState(false)
  const [open, setOpen] = useState<string | null>(null)
  const agents = useReview().agents?.agents ?? []
  const target = `branch:${workspace}/${name}`
  const { threads } = useThreads(workspace && name ? target : null)
  const prNumber = b?.pr && (b.pr.state === 'open' || b.pr.state === null) ? b.pr.number : undefined
  useGithubSync(target, prNumber, !!w && w.kind !== 'remote')

  const run = async (op: WsOp, done: string, keep = false) => {
    setBusy(true)
    try {
      const r = await api.stackEdit(workspace, op, keep ? 'resolve' : false)
      const resolving = resolvingWords(r.result?.resolving)
      if (resolving) {
        toast.ok(`${done} ${resolving.text}`, { undo: false })
        if (resolving.agent) go(agentLink(resolving.agent))
        return
      }
      const stoppedAt = r.result?.conflict?.branch ?? r.result?.restack?.conflict?.branch
      const mergeStop = r.result?.stopped
      if (mergeStop) {
        toast.ok(`Merged ${r.result?.merged?.length ?? 0}, then stopped at ${mergeStop.branch}: ${mergeStop.message}`, { undo: false })
        return
      }
      if (stoppedAt) toast.ok(`${stoppedAt} clashes. Pick what each place should say below.`, { undo: false })
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
  if (w && name === w.trunk) {
    const trunkName = w.trunk.replace(/^origin\//, '')
    return (
      <div className="page rv">
        <div className="rv-head">
          <div>
            <h1 className="rv-title mono">{trunkName}</h1>
            <p className="muted">
              the trunk of {w.name}
              {w.base ? ` · at ${shortSha(w.base)}` : ''}
              {' · '}
              <Link className="rv-code-link" to={codeLink(`branch:${w.id}/${w.trunk}`)}>Browse code</Link>
              {w.kind !== 'remote' && w.this_machine && (
                <>
                  {' · '}
                  <Link className="rv-code-link" to={codeLink(`workspace:${w.id}`)} title={`Every file in ${w.folder} as it is now, edits included`}>
                    Working tree
                  </Link>
                </>
              )}
            </p>
          </div>
        </div>
        {w.kind !== 'remote' && w.branches.length === 0 && (
          <p className="muted rv-empty-lead">Nothing stacked in {w.name} yet. Start an agent on {trunkName}, or start a branch yourself.</p>
        )}
        {w.kind !== 'remote' && <StartAgent workspace={w.id} branch={w.trunk} label={trunkName} canRevise={false} settings={w.agent_settings} />}
        {w.kind !== 'remote' && <NewBranch w={w} />}
        <AgentsFrom agents={agents.filter((a) => a.workspace === w.id && a.start.trunk)} workspace={w.id} branch={w.trunk} />
        {w.kind !== 'remote' ? (
          <AgentSettingsForm workspace={w.id} />
        ) : (
          <p className="muted">Agents in {w.name} run with {settingsLine(w.agent_settings)}.</p>
        )}
      </div>
    )
  }
  // Just created: the workspace list can lag the branch by a refresh.
  if (w && !b && detail.data) return <div className="page rv"><Skeleton rows={4} /></div>
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
  const trunk = w.trunk.replace(/^origin\//, '')
  const prOpen = !!b.pr && (b.pr.state === 'open' || b.pr.state === null)
  const mergeBlocked = b.pr?.status?.checks === 'fail' || b.pr?.status?.review === 'changes_requested'
  // Where it can move: the trunk or another branch of this stack, not its own upstack.
  const targets = [w.trunk, ...w.branches.map((x) => x.name)].filter((t) => canMoveBranch(w, b.name, t))
  return (
    <div className="page rv">
      <div className="rv-head">
        <div>
          <h1 className="rv-title mono">{b.name}</h1>
          <p className="muted">
            on {b.parent === w.trunk ? trunk : b.parent.replace(/^origin\//, '')} · {plural(b.commits, 'commit')}
            {b.current ? " · you're here" : ''}
            {w.kind === 'remote' ? ` · ${w.name} on ${w.machine}` : ''}
            {b.pr ? ` · PR #${b.pr.number} ${prLine(b.pr.state, b.pr.status)}` : ''}
            {w.kind !== 'remote' && (
              <>
                {' · '}
                <Link className="rv-code-link" to={codeLink(`branch:${w.id}/${b.name}`)} title="Browse every file of this branch, its changes marked">
                  Browse code
                </Link>
              </>
            )}
          </p>
        </div>
        {b.conflicts > 0 && <span className="tone tone-conf">{`${b.conflicts} clash${b.conflicts === 1 ? '' : 'es'}`}</span>}
      </div>

      {stopped && w.pending && <StackConflicts w={w} />}
      <Resolver agents={agents} w={w} branch={b.name} />

      {!readOnly && !stopped && (
        <div className="br-acts">
          {!b.current && (
            <button type="button" className="btn btn-primary" disabled={busy} title={`Switch ${w.name} to this branch`} onClick={() => void run({ op: 'checkout', branch: b.name }, `Checked out ${b.name}.`)}>
              {WORDS.checkout}
            </button>
          )}
          {prOpen && (
            <button
              type="button"
              className="btn"
              disabled={busy || mergeBlocked}
              title={mergeBlocked ? 'Fix the failing checks or requested changes first' : HELP.merge(below)}
              onClick={() => void run({ op: 'merge', branch: b.name }, `Merged up to ${b.name}.`)}
            >
              {WORDS.merge}
              {below > 1 ? ` ${below} PRs` : ''}
            </button>
          )}
          <MoreMenu
            busy={busy}
            name={b.name}
            targets={targets.map((t) => ({ value: t, label: t === w.trunk ? trunk : t }))}
            onMove={(onto) => void run({ op: 'move', branch: b.name, onto }, `Moved ${b.name} onto ${onto === w.trunk ? trunk : onto}.`, true)}
            onRename={(new_name) => void run({ op: 'rename', branch: b.name, new_name }, `Renamed ${b.name} to ${new_name}.`)}
            onDelete={() => void run({ op: 'delete', branch: b.name }, `Deleted ${b.name}; the branches on it moved down.`, true)}
          />
        </div>
      )}

      {!readOnly && !stopped && <StartAgent workspace={w.id} branch={b.name} label={b.name} canRevise settings={w.agent_settings} settingsLink={`${branchLink(w.id, w.trunk)}#agent-settings`} />}
      <AgentsFrom agents={agentsOn(agents, w.id, b.name)} workspace={w.id} branch={b.name} />

      <ReviewBar target={target} threads={threads} agent={agentWorkingOn(agents, w.id, b.name)} readOnly={readOnly} pr={prNumber} />
      <ThreadList
        target={target}
        threads={threads}
        onJump={(t) => {
          if (t.anchor) setOpen(t.anchor.path)
          scrollToThread(t.id)
        }}
        readOnly={readOnly}
      />

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
                  <FileComments n={threads.filter((t) => t.anchor?.path === f.path && t.state !== 'resolved').length} />
                  <span className="muted">{open === f.path ? '▾' : '▸'}</span>
                </button>
                {open === f.path && (
                  <DiffView
                    base={ref.git(d.base)}
                    head={ref.git(d.tip)}
                    path={f.path}
                    kind={f.kind}
                    codeRef={readOnly ? undefined : `branch:${w.id}/${b.name}`}
                    comments={{ target, threads, readOnly }}
                  />
                )}
              </div>
            ))}
          </div>
        </section>
      )}
    </div>
  )
}

/** How many open threads a file has, beside its name. */
function FileComments({ n }: { n: number }) {
  if (n === 0) return null
  return (
    <span className="file-ncomments" title={`${n} open comment${n === 1 ? '' : 's'}`}>
      <Icon name="comment" size={12} /> {n}
    </span>
  )
}

/** An agent resolving this branch's clash with its parent: what clashed, how far it is, and where to review it. */
function Resolver({ agents, w, branch }: { agents: Parameters<typeof resolverOf>[0]; w: WorkspaceInfo; branch: string }) {
  const a = resolverOf(agents, w.id, branch)
  if (!a) return null
  const st = agentState(a)
  const parent = (a.conflict?.parent ?? 'its parent').replace(/^origin\//, '')
  const paths = a.conflict?.paths ?? []
  return (
    <section className="conf-box conf-agent" aria-label="Clash being resolved">
      <header>
        <b>
          {branch} clashes with {parent} in {paths.length ? paths.join(', ') : 'some files'}
        </b>
        <span className="muted">
          An agent is resolving it in its own copy; nothing here changes until you accept. · {st.label}
        </span>
      </header>
      <footer className="conf-foot">
        <Link className="btn btn-primary" to={agentLink(a.id)}>
          {a.state === 'ready' || a.state === 'tests_failing' || a.state === 'failed' || a.state === 'stopped' ? 'Review the resolution' : 'Watch the agent'}
        </Link>
      </footer>
    </section>
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
      toast.ok(r.open.length ? `${path}: resolved. ${plural(r.open.length, 'file')} left.` : 'All clashes resolved. Press Continue.', { undo: false })
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
          ? 'Cancelled; every branch is back as it was.'
          : r.outcome === 'stopped'
            ? `The next step clashes too: ${r.conflicts?.branch ?? ''}. Resolve it below.`
            : 'Done; the branches above are updated too.',
        { undo: false },
      )
    } catch (e) {
      toast.error(e)
    } finally {
      setBusy(false)
      refresh()
    }
  }

  const branch = c?.branch ?? w.pending?.branch ?? 'this branch'
  const parent = (c?.parent ?? w.pending?.onto ?? 'its parent').replace(/^origin\//, '')
  const labels = {
    theirs: `Now on ${parent}`,
    ours: `Your change${c?.subject ? ` · “${c.subject}”` : ''}`,
    useTheirs: `Use ${parent}`,
    useOurs: 'Use mine',
  }
  return (
    <section className="conf-box" aria-label="Clashes">
      <header>
        <b>
          Stopped at {branch}: {open.length > 0 ? `${plural(open.length, 'file')} changed on both sides` : 'all clashes resolved'}
        </b>
        <span className="muted">Pick what each place should say, then continue.</span>
      </header>
      {data.error && <ErrorNote error={data.error} />}
      {!c && !data.error && <Skeleton rows={3} />}
      {c &&
        open.map((f) => (
          <HunkList key={f.path} path={f.path} hunks={f.hunks} mine={branch} labels={labels} onChoose={(h, take, text) => choose(f.path, h, take, text)} />
        ))}
      {c && (
        <footer className="conf-foot">
          <button type="button" className="btn btn-primary" disabled={busy || open.length > 0} onClick={() => void finish(false)} title="Commit the result and carry on with the branches above">
            Continue
          </button>
          <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => void finish(true)} title="Put every branch back as it was before">
            Cancel
          </button>
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

/** Rare branch edits, out of the way: move, rename, delete. */
function MoreMenu({
  busy,
  name,
  targets,
  onMove,
  onRename,
  onDelete,
}: {
  busy: boolean
  name: string
  targets: { value: string; label: string }[]
  onMove: (onto: string) => void
  onRename: (to: string) => void
  onDelete: () => void
}) {
  const [mode, setMode] = useState<'menu' | 'rename' | 'delete' | null>(null)
  const [to, setTo] = useState(name)
  if (mode === 'rename')
    return (
      <form
        className="br-more-form"
        onSubmit={(e) => {
          e.preventDefault()
          if (to.trim() && to.trim() !== name) onRename(to.trim())
          setMode(null)
        }}
      >
        <input id="br-rename" className="input mono" value={to} autoFocus onChange={(e) => setTo(e.target.value)} aria-label="New name" />
        <button type="submit" className="btn btn-primary" disabled={busy}>
          Rename
        </button>
        <button type="button" className="btn btn-ghost" onClick={() => setMode(null)}>
          Cancel
        </button>
      </form>
    )
  if (mode === 'delete')
    return (
      <span className="br-more-form">
        <span>Delete {name} and its commits?</span>
        <button
          type="button"
          className="btn btn-danger"
          disabled={busy}
          onClick={() => {
            setMode(null)
            onDelete()
          }}
        >
          Delete
        </button>
        <button type="button" className="btn btn-ghost" onClick={() => setMode(null)}>
          Cancel
        </button>
      </span>
    )
  return (
    <span className="br-more">
      <button type="button" className="btn btn-ghost" aria-expanded={mode === 'menu'} onClick={() => setMode((m) => (m === 'menu' ? null : 'menu'))}>
        More ▾
      </button>
      {mode === 'menu' && (
        <span className="br-menu" role="menu">
          {targets.length > 0 && (
            <label className="br-menu-item">
              Move onto
              <select
                id="br-move"
                defaultValue=""
                disabled={busy}
                onChange={(e) => {
                  if (!e.target.value) return
                  setMode(null)
                  onMove(e.target.value)
                }}
              >
                <option value="" disabled>
                  choose…
                </option>
                {targets.map((t) => (
                  <option key={t.value} value={t.value}>
                    {t.label}
                  </option>
                ))}
              </select>
            </label>
          )}
          <button type="button" role="menuitem" className="br-menu-item" onClick={() => setMode('rename')}>
            Rename…
          </button>
          <button type="button" role="menuitem" className="br-menu-item br-menu-danger" onClick={() => setMode('delete')}>
            Delete…
          </button>
        </span>
      )}
    </span>
  )
}

/** "Or start a branch yourself": a new branch on the trunk, checked out in that workspace. */
function NewBranch({ w }: { w: WorkspaceInfo }) {
  const [name, setName] = useState('')
  const [busy, setBusy] = useState(false)
  const toast = useToast()
  const go = useGo()
  const { refresh } = useLive()
  const trunk = w.trunk.replace(/^origin\//, '')
  const branch = name.trim().replace(/\s+/g, '-')
  const create = async (e: FormEvent) => {
    e.preventDefault()
    if (!branch) return
    setBusy(true)
    try {
      const r = await api.stackEdit(w.id, { op: 'create', branch, parent: w.trunk })
      setName('')
      if (r.outcome === 'queued') toast.ok(`Queued: ${r.reason ?? 'an agent is working there'}. ${branch} starts when it stops.`, { undo: false })
      else {
        toast.ok(`Started ${branch} on ${trunk}, checked out in ${w.name}.`, { undo: undoIn(w.id, `Started ${branch}.`) })
        go(branchLink(w.id, branch))
      }
    } catch (err) {
      toast.error(err)
    } finally {
      setBusy(false)
      refresh()
    }
  }
  return (
    <form className="rv-newbranch" onSubmit={(e) => void create(e)}>
      <label htmlFor={`newbranch-${w.id}`}>Or start a branch yourself</label>
      <input
        id={`newbranch-${w.id}`}
        className="mono"
        value={name}
        onChange={(e) => setName(e.target.value)}
        placeholder="branch-name"
        spellCheck={false}
        autoComplete="off"
      />
      <button type="submit" className="btn" disabled={busy || !branch} title={`Create ${branch || 'the branch'} on ${trunk} and check it out in ${w.folder ?? w.path}. Edits already there come along.`}>
        New branch
      </button>
    </form>
  )
}
