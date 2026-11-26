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

test('the pane draws one tree: main, each window\'s stack, versions under what they are on', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const calls = daemon(on)
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()
  expect(calls.map(c => c.path)).toContain('/api/workspaces')

  for (const surface of SURFACES) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect(await ui.find({ type: 'Text', text: /^demo9$/ })).toBeDefined()
    // main's newest commit leads, named by its subject.
    expect(await ui.find({ type: 'Text', text: 'trunk: sub' })).toBeDefined()
    // Two windows with stacks: each is named, this one first and marked.
    const heads = await ui.findAll({ type: 'Text', text: /^(main-window|docs)$/ })
    expect([...new Set(heads.map(h => h.text))]).toEqual(['main-window', 'docs'])
    expect(await ui.find({ type: 'Text', text: /this window/ })).toBeDefined()
    for (const name of ['mul', 'div', 'cli-ops']) expect(await ui.find({ key: rowKey.branch(MAIN_WS, name) })).toBeDefined()
    expect(await ui.find({ key: rowKey.branch(DOCS_WS, 'usage-docs') })).toBeDefined()
    expect((await ui.find({ key: rowKey.branch(MAIN_WS, 'mul') }))?.text).toContain('needs restack')
    // Versions: claude's on an older main commit (one behind), codex's on div.
    expect((await ui.find({ key: rowKey.version(2) }))?.text).toContain('↓1')
    expect((await ui.find({ key: rowKey.version(3) }))?.text).toContain('Tests fail')
    // A version on a branch tip is not "behind main".
    expect((await ui.find({ key: rowKey.version(3) }))?.text).not.toContain('↓')
    // The old sections are gone: one tree, no separate lists.
    expect(await ui.find({ type: 'Text', text: /RUNNING|TASKS ·/ })).toBeUndefined()
    await ui.unmount()
  }
})

test('pressing a version shows its actions, each with what it does, and its diff', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const calls = daemon(on)
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: rowKey.version(2) })
  await clock.settle()
  expect(calls.map(c => c.path)).toContain('/api/diff?b=a2')
  for (const key of ['choose', 'update-main', 'rebase-onto', 'open-pr', 'discard']) expect(await ui.find({ key })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /Pick this as the version to ship/ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /Main has 1 new commit/ })).toBeDefined()
  const diff = await ui.find({ type: 'Code', text: /@@ / })
  expect(diff).toBeDefined()
  expect(await ui.find({ type: 'Link', text: 'Open in the browser' })).toBeDefined()

  // Choose posts to the daemon.
  await ui.press({ key: 'choose' })
  expect(calls.some(c => c.method === 'POST' && c.path === '/api/attempts/2/promote')).toBe(true)
  await ui.unmount()
})

test('Rebase onto… picks a target by press, previews, then rebases', async ($, on) => {
  const clock = mock.clock(on)
  const toasts = engine(on)
  const calls = daemon(on, {
    posts: {
      '/api/attempts/2/rebase': body =>
        (body as { preview?: boolean }).preview
          ? { rebase: { changes: [{ path: 'calc/fmt.py', kind: 'added' }], conflict_files: [], rebased: true, preview: true, resolved: [] } }
          : { rebase: { changes: [], conflicts: [], rebased: true }, attempt: {} },
    },
  })
  await $.session.start({ cwd: HERE, surface: 'desktop', isInteractive: true })
  await clock.settle()

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: rowKey.version(2) })
  await ui.press({ key: 'rebase-onto' })
  // The newest main commit is a target; the one it is already on is not.
  const head = (FIXTURE['/api/graph'] as { trunk: { commit: string }[] }).trunk[0]!.commit
  expect(await ui.find({ key: `drop-c${head}` })).toBeDefined()
  // Stacking on codex's version is allowed too.
  expect(await ui.find({ key: 'drop-v3' })).toBeDefined()
  await ui.press({ key: `drop-c${head}` })
  const preview = calls.find(c => c.method === 'POST' && c.path === '/api/attempts/2/rebase')
  expect(preview?.body).toEqual({ onto: head, preview: true })
  expect(await ui.find({ type: 'Text', text: /Rebase claude onto trunk: sub\?/ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /Merges cleanly/ })).toBeDefined()

  await ui.press({ key: 'apply-move' })
  const applied = calls.filter(c => c.method === 'POST' && c.path === '/api/attempts/2/rebase').at(-1)
  expect(applied?.body).toEqual({ onto: head, no_fetch: true, keep_conflicts: false })
  expect(toasts).toContain('Rebased claude onto trunk: sub.')
  await ui.unmount()
})

test('a branch restacks, and moves onto another branch by press', async ($, on) => {
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
  expect(await ui.find({ type: 'Text', text: /Replays this branch, and the branches above it/ })).toBeDefined()
  await ui.press({ key: 'restack' })
  expect(calls.find(c => c.method === 'POST')?.body).toEqual({ op: 'restack', branch: 'mul', keep_conflicts: true })

  await ui.press({ key: rowKey.branch(MAIN_WS, 'cli-ops') })
  await ui.press({ key: 'move-onto' })
  // Its own parent and other windows' branches are not targets.
  expect(await ui.find({ key: `drop-w${MAIN_WS}-b-div` })).toBeUndefined()
  expect(await ui.find({ key: `drop-w${DOCS_WS}-b-usage-docs` })).toBeUndefined()
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
  expect((await ui.find({ key: rowKey.version(2) }))?.text).toContain('1 conflict')
  await ui.press({ key: rowKey.version(2) })
  await clock.settle()
  expect(await ui.find({ type: 'Code', text: 'return str(x)\n' })).toBeDefined()
  expect((await ui.find({ key: 'keep-theirs-calc/fmt.py-1' }))?.text).toBe('Keep main')
  expect((await ui.find({ key: 'keep-ours-calc/fmt.py-1' }))?.text).toBe('Keep claude')
  // No moves while it clashes.
  expect(await ui.find({ key: 'rebase-onto' })).toBeUndefined()
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
