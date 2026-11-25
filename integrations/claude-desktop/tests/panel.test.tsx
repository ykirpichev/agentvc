import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

import { FIXTURE } from './fixture'

const PANE = {
  plugin: 'agentvc-panel',
  component: 'Pane',
  requestId: 'agentvc',
  props: {
    title: 'agentvc',
    isFocused: false,
    bodyColumns: 48,
    placement: 'dock',
    scroll: { offset: 0, bodyRows: 60 },
    view: {},
  },
} as const

const SURFACES = ['terminal', 'desktop'] as const

/** Stands for agentvcd: answers from the recorded fixture, or is down. */
function daemon(on: On, opts: { isDown?: boolean } = {}) {
  const calls: string[] = []
  on('http.fetch', (_, e) => {
    const url = new URL(e.url)
    calls.push(`${e.init?.method ?? 'GET'} ${url.pathname}`)
    if (opts.isDown) throw new Error('connection refused')
    if (url.pathname === '/api/attempts/a8/stop') {
      return { value: { status: 200, ok: true, headers: {}, text: '{"run":1,"pid":1,"signal":"TERM"}' } }
    }
    const body = FIXTURE[url.pathname]
    return {
      value:
        body === undefined
          ? { status: 404, ok: false, headers: {}, text: '{"error":{"code":"no_route","message":"no"}}' }
          : { status: 200, ok: true, headers: {}, text: JSON.stringify(body) },
    }
  })
  return calls
}

function engine(on: On) {
  on('command.register', () => ({ value: { command: 'agentvc' } }))
  on('ui.open', () => ({ value: { isPlaced: true } }))
  on('ui.toast', () => ({ value: undefined }))
  on('ui.status', () => ({ value: undefined }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
}

test('the pane shows running agents and open tasks', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const calls = daemon(on)
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await clock.settle()
  expect(calls).toContain('GET /api/live')

  for (const surface of SURFACES) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect((await ui.find({ type: 'Text', text: /^demo$/ }))?.text).toBe('demo')
    expect((await ui.find({ type: 'Text', text: /RUNNING/ }))?.text).toBe('RUNNING · 1')
    // a8 runs under `agentvc run`, so it has a Stop button.
    expect(await ui.find({ key: 'stop-a8' })).toBeDefined()
    // t2 is open; t1 has a winner, so it sits under "settled".
    expect(await ui.find({ key: 't2' })).toBeDefined()
    expect(await ui.find({ key: 't1' })).toBeUndefined()
    expect((await ui.find({ key: 'toggle-done' }))?.text).toBe('Show 1 settled')
    await ui.unmount()
  }
})

test('expanding a settled task draws its attempt tree with reasons', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const calls = daemon(on)
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await clock.settle()

  for (const surface of SURFACES) {
    const ui = await $.ui.mount({ ...PANE, surface })
    if ((await ui.find({ key: 't1' })) === undefined) await ui.press({ key: 'toggle-done' })
    if ((await ui.find({ key: 'open-a3' })) === undefined) await ui.press({ key: 't1' })
    await clock.settle()
    expect(calls).toContain('GET /api/tasks/1/stats')

    const winner = await ui.find({ key: 'open-a3' })
    expect(winner?.text).toContain('winner')
    expect((await ui.find({ key: 'open-a4' }))?.text).toContain('abandoned')
    expect(await ui.find({ type: 'Text', text: /rewrote auth module, too broad/ })).toBeDefined()
    // a7 was forked from a2, so it hangs one level deeper.
    expect(await ui.find({ type: 'Text', text: /^│  └─ $/ })).toBeDefined()
    await ui.unmount()
  }
})

test('Stop posts to the daemon', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  const calls = daemon(on)
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await clock.settle()

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'stop-a8' })
  expect(calls).toContain('POST /api/attempts/a8/stop')
  await ui.unmount()
})

test('a daemon that does not answer says how to start it', async ($, on) => {
  const clock = mock.clock(on)
  engine(on)
  daemon(on, { isDown: true })
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
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
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await clock.settle()

  const ui = await $.ui.mount({ ...PANE, props: { ...PANE.props, placement: 'widget' }, surface: 'desktop' })
  expect(await ui.find({ type: 'Text', text: /1 running/ })).toBeDefined()
  await ui.unmount()
})
