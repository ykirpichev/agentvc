import { Link, useNavigate, useParams, useSearchParams } from 'react-router'
import { api, ref } from '../api/client'
import type { AttemptDetail, Ref } from '../api/types'
import { ActionButtons, useAttemptActions } from '../components/ReviewActions'
import { useResource } from '../api/useResource'
import { AttemptMeta } from '../components/AttemptMeta'
import { ChangeList } from '../components/ChangeList'
import { DiffView } from '../components/DiffView'
import { Empty, ErrorNote, Kbd, PageHeader, Skeleton, StatusPill } from '../components/bits'
import { Icon } from '../components/Icon'
import { useHotkeys } from '../keyboard/hotkeys'
import { useListNavigation } from '../keyboard/useListNavigation'
import { actorName, timeAgo } from '../lib/format'

export function AttemptPage() {
  const id = Number(useParams().id)
  const { data, error, loading } = useResource(`attempt:${id}`, () => api.attempt(id))
  return (
    <div className="page page-wide">
      {error && <ErrorNote error={error} />}
      {loading && <Skeleton />}
      {data && <AttemptView key={id} detail={data} />}
    </div>
  )
}

function AttemptView({ detail }: { detail: AttemptDetail }) {
  const { attempt, head } = detail
  const navigate = useNavigate()
  const [params, setParams] = useSearchParams()
  const snapshots = useResource(`snapshots:${attempt.id}`, () => api.snapshots(attempt.id))
  const siblings = useResource(attempt.task_id ? `attempts:t${attempt.task_id}` : null, () =>
    api.attempts(attempt.task_id ?? undefined),
  )
  const actions = useAttemptActions(attempt, siblings.data ?? [], { group: 'Attempt' })

  // Compare against the commit it was rebased onto, else the fork point; the
  // root attempt has neither, so it shows each snapshot against the one before.
  const snapParam = params.get('snap')
  const headRef: Ref | null = snapParam
    ? ref.snapshot(Number(snapParam))
    : attempt.parent_snapshot
      ? ref.attempt(attempt.id)
      : head
        ? ref.snapshot(head.id)
        : null
  // A snapshot from before the rebase still compares with the fork point.
  const gitBase =
    detail.git_base && !(snapParam && Number(snapParam) < detail.git_base.snapshot_id) ? detail.git_base : null
  const baseRef: Ref | undefined = gitBase
    ? ref.git(gitBase.commit)
    : attempt.parent_snapshot
      ? ref.snapshot(attempt.parent_snapshot)
      : undefined
  const diff = useResource(headRef ? `diff:${baseRef}..${headRef}` : null, () => api.diff(headRef!, baseRef))
  const changes = diff.data?.changes ?? []

  const fileParam = params.get('file')
  const fileIndex = Math.max(0, changes.findIndex((c) => c.path === fileParam))
  const select = (i: number) => {
    const c = changes[i]
    if (!c) return
    const next = new URLSearchParams(params)
    next.set('file', c.path)
    setParams(next, { replace: true })
  }
  const nav = useListNavigation(changes.length, {
    group: 'Files',
    noun: 'file',
    selected: fileIndex,
    onSelect: select,
  })
  const selected = nav.selected
  const upTo = attempt.task_id ? `/tasks/${attempt.task_id}` : '/tasks'
  useHotkeys([
    { keys: ['u', 'backspace'], description: 'Up to the task', group: 'Attempt', handler: () => navigate(upTo) },
  ])
  const current = changes[selected]

  return (
    <>
      <Link to={upTo} className="back">
        <Icon name="back" size={14} /> {attempt.task_id ? `Task t${attempt.task_id}` : 'Tasks'}
      </Link>
      <PageHeader
        eyebrow={
          <>
            a{attempt.id} · {attempt.backend} view · updated {head ? timeAgo(head.created_at) : 'never'}
          </>
        }
        title={
          <span className="title-with-pill">
            {attempt.id === 1 ? 'Working directory' : actorName(attempt.actor)}
            <StatusPill status={attempt.status} />
          </span>
        }
      >
        {snapshots.data && snapshots.data.length > 0 && (
          <label className="select">
            <span>Snapshot</span>
            <select
              value={snapParam ?? ''}
              onChange={(e) => {
                const next = new URLSearchParams(params)
                if (e.target.value) next.set('snap', e.target.value)
                else next.delete('snap')
                setParams(next, { replace: true })
              }}
            >
              <option value="">Latest</option>
              {[...snapshots.data].reverse().map((s) => (
                <option key={s.id} value={s.id}>
                  s{s.id} · {s.trigger}
                  {s.tags?.length ? ` · ${s.tags.map((t) => `#${t}`).join(' ')}` : ''} · {timeAgo(s.created_at)}
                </option>
              ))}
            </select>
          </label>
        )}
        <ActionButtons actions={actions} />
      </PageHeader>
      <AttemptMeta
        attempt={attempt}
        extra={
          <>
            <div>
              <dt>Snapshots</dt>
              <dd>{detail.snapshot_count}</dd>
            </div>
            {detail.base_commit && (
              <div>
                <dt>Based on</dt>
                <dd title={detail.base_commit}>
                  <code>{detail.base_commit.slice(0, 7)}</code>
                  {gitBase && <span className="rebased-tag">rebased</span>}
                </dd>
              </div>
            )}
            {detail.pr_url && (
              <div>
                <dt>Pull request</dt>
                <dd>
                  <a href={detail.pr_url} target="_blank" rel="noreferrer">
                    {detail.pr_url.replace(/^https:\/\/github\.com\//, '')}
                  </a>
                </dd>
              </div>
            )}
            {detail.landed && (
              <div>
                <dt>Landed</dt>
                <dd title={detail.landed.commit}>
                  on <code>{detail.landed.branch}</code> at{' '}
                  <code>{detail.landed.commit.slice(0, 7)}</code>
                </dd>
              </div>
            )}
          </>
        }
      />

      {diff.error && <ErrorNote error={diff.error} />}
      {diff.data && changes.length === 0 && (
        <Empty title="No changes">
          {gitBase
            ? `Identical to ${gitBase.commit.slice(0, 7)}, the commit it was rebased onto.`
            : baseRef
              ? `Identical to ${baseRef}, where it was forked from.`
              : 'Nothing changed in this snapshot.'}
        </Empty>
      )}
      {diff.data && changes.length > 0 && (
        <div className="review">
          <aside className="panel files">
            <div className="panel-head">
              <span>
                {refLabel(diff.data.a_ref)} → s{diff.data.b.id}
              </span>
              <span className="muted">{changes.length} files</span>
            </div>
            <ChangeList changes={changes} selected={selected} onSelect={select} rowRef={nav.rowRef} />
            <div className="panel-foot muted">
              <Kbd keys="j" /> <Kbd keys="k" /> next / previous file
            </div>
          </aside>
          {current && (
            <DiffView
              key={current.path}
              base={diff.data.a_ref}
              head={ref.snapshot(diff.data.b.id)}
              path={current.path}
              kind={current.kind}
            />
          )}
        </div>
      )}
      {actions.node}
    </>
  )
}

/** `git:<sha>` shortened for headers; snapshot refs as they are. */
function refLabel(r: Ref): string {
  return r.startsWith('git:') ? r.slice(4, 11) : r
}
