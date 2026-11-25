import * as assert from 'node:assert/strict'
import { AgentvcError, CliBackend, DaemonBackend, type CliRunner } from '../../src/client'

function fakeFetch(routes: Record<string, [number, unknown]>, seen: string[] = []): typeof fetch {
  return (async (input: string | URL | Request, init?: RequestInit) => {
    const url = String(input)
    seen.push(`${init?.method ?? 'GET'} ${url}${init?.body ? ` ${String(init.body)}` : ''}`)
    const key = Object.keys(routes).find((k) => url.endsWith(k))
    const [status, body] = key ? routes[key] : [404, { error: { code: 'not_found', message: 'no route' } }]
    return new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
  }) as typeof fetch
}

describe('DaemonBackend', () => {
  it('builds API URLs and posts actions as JSON', async () => {
    const seen: string[] = []
    const d = new DaemonBackend('http://127.0.0.1:7420/', fakeFetch({
      '/api/diff?a=s1&b=a3': [200, { changes: [{ path: 'x', kind: 'added' }] }],
      '/api/attempts/3/abandon': [200, { attempt: { id: 3 }, closed_siblings: [] }],
    }, seen))
    assert.deepEqual(await d.diff('a3', 's1'), [{ path: 'x', kind: 'added' }])
    await d.abandon(3, 'dead end')
    assert.equal(seen[1], 'POST http://127.0.0.1:7420/api/attempts/3/abandon {"reason":"dead end"}')
    assert.equal(d.eventsUrl, 'ws://127.0.0.1:7420/api/events')
  })

  it('returns null for a file missing at a ref, and throws stable codes otherwise', async () => {
    const d = new DaemonBackend('http://x', fakeFetch({
      'ref=a2&path=gone.txt': [404, { error: { code: 'not_found', message: 'file a2:gone.txt not found' } }],
      'ref=a9&path=x': [404, { error: { code: 'not_found', message: 'attempt a9 not found' } }],
    }))
    assert.equal(await d.file('a2', 'gone.txt'), null)
    await assert.rejects(d.file('a9', 'x'), (e: AgentvcError) => e.code === 'not_found')
  })

  it('reports an unreachable daemon as offline', async () => {
    const d = new DaemonBackend('http://x', (async () => {
      throw new TypeError('fetch failed')
    }) as typeof fetch)
    await assert.rejects(d.space(), (e: AgentvcError) => e.code === 'offline')
  })
})

describe('CliBackend', () => {
  it('maps CLI JSON to the daemon shapes', async () => {
    const calls: string[][] = []
    const attempts = [
      { id: 1, task_id: null, parent_attempt: null, status: 'exploring', created_at: 5, view_path: '/r' },
      { id: 2, task_id: 1, parent_attempt: 1, status: 'passed', created_at: 9, view_path: '/r/.agentvc/views/a2' },
    ]
    const run: CliRunner = async (args) => {
      calls.push(args)
      if (args[0] === 'log') return { ok: true, attempts }
      if (args[0] === 'task') return { ok: true, tasks: [{ id: 1, goal: 'g', criteria: null, created_at: 1 }] }
      if (args[0] === 'promote') return { ok: true, attempt: attempts[1], closed_siblings: [3] }
      throw new AgentvcError('cli', 'unexpected')
    }
    const c = new CliBackend(run)
    const [t] = await c.tasks()
    assert.equal(t.attempts.passed, 1)
    assert.equal(t.updated_at, 9)
    assert.equal((await c.space()).workdir, '/r')
    const r = await c.promote(2)
    assert.deepEqual(r.closed_siblings, [3])
    assert.deepEqual(calls.at(-1), ['promote', 'a2'])
    await assert.rejects(c.file('s4', 'x'), (e: AgentvcError) => e.code === 'needs_daemon')
  })
})
