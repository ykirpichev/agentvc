import { STATUSES, type QueryParams, type Status } from '../api/types'

const csv = (s: string | null) =>
  (s ?? '')
    .split(',')
    .map((x) => x.trim())
    .filter(Boolean)

/** The query the URL describes; the URL is the only state, so filters survive reload and links. */
export function queryFromParams(p: URLSearchParams): QueryParams {
  const task = p.get('task')
  return {
    task: task ? Number(task.replace(/^t/, '')) : undefined,
    status: csv(p.get('status')).filter((s): s is Status => (STATUSES as readonly string[]).includes(s)),
    actor: p.get('actor') ?? undefined,
    model: p.get('model') ?? undefined,
    touched: csv(p.get('touched')),
    since: p.get('since') ?? undefined,
    meta: csv(p.get('meta')),
    grep: p.get('grep') ?? undefined,
  }
}

/** Link to the Find page with these filters. */
export function findLink(q: { task?: number | null; touched?: string; status?: Status[] }): string {
  const p = new URLSearchParams()
  if (q.task) p.set('task', `t${q.task}`)
  if (q.touched) p.set('touched', q.touched)
  if (q.status?.length) p.set('status', q.status.join(','))
  return `/find?${p}`
}
