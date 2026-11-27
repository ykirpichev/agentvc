import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

import { FIXTURE } from './fixture'
import { unifiedDiff } from '../hooks/model'

const PANE = {
  plugin: 'agentvc-panel',
  component: 'Pane',
  requestId: 'agentvc',
  props: {
    title: 'agentvc',
    isFocused: false,
    bodyColumns: 60,
    placement: 'dock',
    scroll: { offset: 0, bodyRows: 80 },
    view: {},
  },
} as const

const SURFACES = ['terminal', 'desktop'] as const
const HERE = '/home/me/demo9'
const MAIN_WS = 'w3247c1f6'
const DOCS_WS = 'wf41d4ab4'

type Call = { method: string; path: string; body: unknown }
type Answer = (body: unknown) => unknown

/** Stands for agentvcd: answers from the recorded fixture, or is down. */
function daemon(
  on: On,
  opts: { isDown?: boolean; posts?: Record<string, Answer>; gets?: Record<string, unknown> } = {},
) {
  const calls: Call[] = []
  on('http.fetch', (_, e) => {
    const url = new URL(e.url)
    const path = url.pathname + url.search
    const method = e.init?.method ?? 'GET'
    const body = typeof e.init?.body === 'string' ? (JSON.parse(e.init.body) as unknown) : undefined
    calls.push({ method, path, body })
    if (opts.isDown) throw new Error('connection refused')
    const ok = (v: unknown) => ({ value: { status: 200, ok: true, headers: {}, text: JSON.stringify(v) } })
    if (method === 'POST') {
      const answer = opts.posts?.[url.pathname]
      return answer
        ? ok(answer(body))
        : { value: { status: 400, ok: false, headers: {}, text: '{"error":{"code":"nope","message":"not in this test"}}' } }
    }
    const v = opts.gets?.[path] ?? FIXTURE[path]
    return v === undefined
      ? { value: { status: 404, ok: false, headers: {}, text: '{"error":{"code":"not_found","message":"no"}}' } }
      : ok(v)
  })
  return calls
}

function engine(on: On) {
  const toasts: string[] = []
  on('command.register', () => ({ value: { command: 'agentvc' } }))
  on('ui.open', () => ({ value: { isPlaced: true } }))
  on('ui.toast', (_, e) => {
    toasts.push(String((e as { text?: unknown }).text ?? ''))
    return { value: undefined }
  })
  on('ui.status', () => ({ value: undefined }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  return toasts
}

const rowKey = {
  trunk: (sha: string) => `row-c${sha}`,
  version: (id: number) => `row-v${id}`,
  branch: (ws: string, name: string) => `row-w${ws}-b-${name}`,
}

test('the pane draws this window\'s stack like gt log, other windows in a line, then agent work', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const calls = daemon(on)
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()
  expect(calls.map(c => c.path)).toContain('/api/workspaces')

  for (const surface of SURFACES) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect(await ui.find({ type: 'Text', text: /^demo9$/ })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /this window/ })).toBeDefined()
    // This window's branches, newest on top: cli-ops and div above mul, main last.
    const order = ['cli-ops', 'div', 'mul'].map(async n => (await ui.find({ key: rowKey.branch(MAIN_WS, n) })) !== undefined)
    expect(await Promise.all(order)).toEqual([true, true, true])
    // Main's commit list is gone; the other window is one line.
    expect(await ui.find({ type: 'Text', text: 'trunk: sub' })).toBeUndefined()
    expect(await ui.find({ key: rowKey.branch(DOCS_WS, 'usage-docs') })).toBeUndefined()
    expect(await ui.find({ key: `row-win-${DOCS_WS}` })).toBeDefined()
    // Agent work: the task's tries, with their status.
    expect(await ui.find({ type: 'Text', text: 'AGENT WORK' })).toBeDefined()
    expect((await ui.find({ key: rowKey.version(3) }))?.text).toContain('Tests fail')
    // The stack's own buttons.
    for (const key of ['sync', 'stack-undo']) expect(await ui.find({ key })).toBeDefined()
    await ui.unmount()
  }
})

test('pressing a try shows its actions and its diff', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const calls = daemon(on)
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: rowKey.version(2) })
  await clock.settle()
  expect(calls.map(c => c.path)).toContain('/api/diff?b=a2')
  for (const key of ['choose', 'update-main', 'open-pr', 'discard']) expect(await ui.find({ key })).toBeDefined()
  expect((await ui.find({ key: 'choose' }))?.text).toBe('Use this')
  expect(await ui.find({ key: 'rebase-onto' })).toBeUndefined()
  const diff = await ui.find({ type: 'Code', text: /@@ / })
  expect(diff).toBeDefined()
  expect(await ui.find({ type: 'Link', text: 'Open in the browser' })).toBeDefined()

  await ui.press({ key: 'choose' })
  expect(calls.some(c => c.method === 'POST' && c.path === '/api/attempts/2/promote')).toBe(true)
  await ui.unmount()
})

test('a version shows its recorded tests, cost and notes', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const base = FIXTURE['/api/graph'] as { attempts: { attempt: { id: number } }[] }
  const graph = {
    ...base,
    attempts: base.attempts.map(g =>
      g.attempt.id === 2
        ? {
            ...g,
            tests: { name: 'tests', passed: true, stale: true, snapshot: 3, summary: 'pass · 12 passed · 3s' },
            agent: { models: [], cost_usd: 0.42, tokens_in: 120000, tokens_out: 8000, notes: [{ text: 'tried the easy fix first', actor: 'agent:claude', created_at: 1 }], transcripts: [], annotations: 2 },
          }
        : g,
    ),
  }
  daemon(on, { gets: { '/api/graph': graph } })
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: rowKey.version(2) })
  await clock.settle()
  expect(await ui.find({ type: 'Text', text: 'Tests passed before the latest changes · $0.42 · 128k tokens' })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: 'tried the easy fix first' })).toBeDefined()
  await ui.unmount()
})

test('Sync and Submit stack act on this window', async ($, on) => {
  const clock = mock.clock(on)
  const toasts = engine(on)
  const calls = daemon(on, {
    posts: {
      [`/api/workspaces/${MAIN_WS}/stack`]: body =>
        (body as { op: string }).op === 'submit'
          ? { outcome: 'applied', request_id: 'r2', workspace: MAIN_WS, result: { pushed: ['mul', 'div'], prs: [{ branch: 'mul', number: 7, url: 'u' }] } }
          : { outcome: 'applied', request_id: 'r1', workspace: MAIN_WS, result: { restack: { conflict: null } } },
    },
  })
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'sync' })
  expect(calls.find(c => c.method === 'POST')?.body).toEqual({ op: 'sync', keep_conflicts: true })
  // mul needs a restack in the fixture, so Submit waits for a Sync.
  expect(await ui.find({ key: 'submit' })).toBeUndefined()
  expect(toasts.some(t => t.startsWith('Synced main-window'))).toBe(true)
  await ui.unmount()
})

test('a branch checks out, and moves onto another branch by press', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const calls = daemon(on, {
    posts: { [`/api/workspaces/${MAIN_WS}/stack`]: () => ({ outcome: 'applied', request_id: 'r1', workspace: MAIN_WS }) },
  })
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: rowKey.branch(MAIN_WS, 'mul') })
  await clock.settle()
  expect(await ui.find({ key: 'restack' })).toBeUndefined()
  await ui.press({ key: 'checkout' })
  expect(calls.find(c => c.method === 'POST')?.body).toEqual({ op: 'checkout', branch: 'mul' })

  await ui.press({ key: rowKey.branch(MAIN_WS, 'cli-ops') })
  await ui.press({ key: 'move-onto' })
  // Its own parent is not a target.
  expect(await ui.find({ key: `drop-w${MAIN_WS}-b-div` })).toBeUndefined()
  await ui.press({ key: `drop-w${MAIN_WS}-b-mul` })
  expect(calls.filter(c => c.method === 'POST').at(-1)?.body).toEqual({ op: 'move', branch: 'cli-ops', onto: 'mul', keep_conflicts: true })
  await ui.unmount()
})

test('a clash shows both sides with Keep buttons', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const graph = structuredClone(FIXTURE['/api/graph']) as { attempts: { attempt: { id: number }; conflicts: number }[] }
  graph.attempts.find(g => g.attempt.id === 2)!.conflicts = 1
  const calls = daemon(on, {
    gets: {
      '/api/graph': graph,
      '/api/attempts/2/conflicts': {
        attempt: {},
        conflicts: [],
        files: [
          {
            path: 'calc/fmt.py',
            text: '',
            hunks: [{ index: 1, line: 3, ours: 'return f"{x:.2f}"\n', base: null, theirs: 'return str(x)\n', ours_label: 'a2', theirs_label: 'origin/main' }],
          },
        ],
      },
    },
    posts: { '/api/attempts/2/conflicts/resolve': () => ({ resolve: { path: 'calc/fmt.py', resolved_hunks: 1, remaining_hunks: 0, open: [] }, attempt: {} }) },
  })
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  expect((await ui.find({ key: rowKey.version(2) }))?.text).toContain('1 clash')
  await ui.press({ key: rowKey.version(2) })
  await clock.settle()
  expect(await ui.find({ type: 'Code', text: 'return str(x)\n' })).toBeDefined()
  expect((await ui.find({ key: 'keep-theirs-calc/fmt.py-1' }))?.text).toBe('Keep main')
  expect((await ui.find({ key: 'keep-ours-calc/fmt.py-1' }))?.text).toBe('Keep claude')
  await ui.press({ key: 'keep-theirs-calc/fmt.py-1' })
  expect(calls.find(c => c.method === 'POST')?.body).toEqual({ path: 'calc/fmt.py', hunks: [{ index: 1, take: 'theirs' }] })
  await ui.unmount()
})

test('a daemon that does not answer says how to start it', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  daemon(on, { isDown: true })
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()

  for (const surface of SURFACES) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect(await ui.find({ type: 'Text', text: /not answering/ })).toBeDefined()
    expect(await ui.find({ key: 'retry' })).toBeDefined()
    await ui.unmount()
  }
})

test('the widget is a one-glance summary', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  daemon(on)
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()

  const ui = await $.ui.mount({ ...PANE, props: { ...PANE.props, placement: 'widget' }, surface: 'desktop' })
  expect(await ui.find({ type: 'Text', text: /0 running/ })).toBeDefined()
  // mul needs a restack.
  expect(await ui.find({ type: 'Text', text: /1 need attention/ })).toBeDefined()
  await ui.unmount()
})

test('unifiedDiff draws hunks with context', async () => {
  const before = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j'].join('\n') + '\n'
  const after = ['a', 'b', 'c', 'd', 'E', 'f', 'g', 'h', 'i', 'j', 'k'].join('\n') + '\n'
  expect(unifiedDiff(before, after)).toBe(
    ['@@ -2,9 +2,10 @@', ' b', ' c', ' d', '-e', '+E', ' f', ' g', ' h', ' i', ' j', '+k'].join('\n'),
  )
  expect(unifiedDiff('', 'x\n')).toBe('@@ -0,0 +1,1 @@\n+x')
  expect(unifiedDiff('same\n', 'same\n')).toBe('')
})
