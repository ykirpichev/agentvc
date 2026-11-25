import * as assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import * as path from 'node:path'
import * as vscode from 'vscode'
import type { AgentvcApi } from '../../src/extension'
import type { Node } from '../../src/tree'
import type { Attempt } from '../../src/types'

const bin = process.env.AGENTVC_BIN_DIR!
const fixture = process.env.AGENTVC_FIXTURE!

function cli(...args: string[]): Record<string, any> {
  return JSON.parse(execFileSync(path.join(bin, 'agentvc'), ['--json', ...args], { cwd: fixture, encoding: 'utf8' }))
}

async function until<T>(what: string, fn: () => T | undefined | Promise<T | undefined>, ms = 10_000): Promise<T> {
  const end = Date.now() + ms
  for (;;) {
    const v = await fn()
    if (v) return v
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`)
    await new Promise((r) => setTimeout(r, 100))
  }
}

const attemptNode = (attempt: Attempt): Node => ({ kind: 'attempt', node: { attempt, children: [] } })

describe('agentvc extension', function () {
  let api: AgentvcApi

  before(async () => {
    const ext = vscode.extensions.getExtension<AgentvcApi>('agentvc.agentvc')!
    api = await ext.activate()
    await until('connection', () => api.connection.state === 'connected')
    await until('attempts', () => api.tree.allAttempts.length >= 5)
  })

  it('starts its own daemon and shows tasks → attempts', async () => {
    assert.ok(api.connection.daemon, 'uses agentvcd, not the CLI fallback')
    const roots = await api.tree.getChildren()
    const tasks = roots.filter((n) => n.kind === 'task')
    assert.deepEqual(tasks.map((t) => (t.kind === 'task' ? t.task.goal : '')).sort(), [
      'Make a.txt better',
      'Reference task',
    ])
    const t1 = tasks.find((t) => t.kind === 'task' && t.task.id === 1)!
    const attempts = await api.tree.getChildren(t1)
    assert.deepEqual(attempts.map((a) => (a.kind === 'attempt' ? a.node.attempt.id : 0)), [2, 3])
    const item = api.tree.getTreeItem(attempts[0])
    assert.match(String(item.description), /^passed · 1 snap · /)
    assert.equal(item.contextValue, 'attempt:open')
    // Untasked a1 is listed too.
    assert.ok(roots.some((n) => n.kind === 'loose'))
  })

  it('opens added, removed and changed files in the diff editor', async () => {
    const r = (await vscode.commands.executeCommand('agentvc.compareWithParent', attemptNode(api.tree.allAttempts.find((a) => a.id === 2)!))) as Awaited<ReturnType<AgentvcApi['commands']['openCompare']>>
    assert.deepEqual(r.entries.map((e) => [e.path, e.kind]), [
      ['a.txt', 'modified'],
      ['gone.txt', 'removed'],
      ['new.txt', 'added'],
    ])
    const text = async (u: vscode.Uri) => (await vscode.workspace.openTextDocument(u)).getText()
    const [mod, rem, add] = r.resources
    assert.equal(await text(mod[1]), 'one\ntwo\nthree\n')
    assert.equal(await text(mod[2]), 'one\nTWO\nthree\n')
    assert.equal(await text(rem[1]), 'remove me\n')
    assert.equal(await text(rem[2]), '')
    assert.equal(await text(add[1]), '')
    assert.equal(await text(add[2]), 'brand new\n')
    // The diff editor opened (multi-file where available, else one diff).
    await until('a diff tab', () =>
      vscode.window.tabGroups.all.some((g) => g.tabs.some((t) => t.input instanceof vscode.TabInputTextDiff || /↔|changes/.test(t.label))),
    )
  })

  it('compares two attempts', async () => {
    const r = await api.commands.compareAttempts(attemptNode(api.tree.allAttempts.find((a) => a.id === 3)!), 'a2')
    assert.ok(r)
    assert.deepEqual(r.entries.map((e) => [e.path, e.kind]), [
      ['a.txt', 'modified'],
      ['gone.txt', 'added'],
      ['new.txt', 'removed'],
    ])
    assert.equal((await vscode.workspace.openTextDocument(r.resources[0][2])).getText(), 'one\n2\nthree\n')
  })

  it('refreshes live when an agent forks and snapshots', async () => {
    const before = api.tree.allAttempts.length
    const started = Date.now()
    const created = cli('fork', '--task', 't1', '--from', 'a1', '--backend', 'copy', '--actor', 'agent:live').fork.attempt
    await until('new attempt in tree', () => api.tree.allAttempts.find((a) => a.id === created.id), 5000)
    assert.equal(api.tree.allAttempts.length, before + 1)
    assert.ok(Date.now() - started < 5000)
    // Abandon it again so promotion below has a predictable sibling set.
    cli('abandon', `a${created.id}`, '--reason', 'test cleanup')
    await until('abandoned in tree', () => api.tree.allAttempts.find((a) => a.id === created.id && a.status === 'abandoned'))
  })

  it('promotes like `agentvc promote`', async () => {
    // Reference: promote t2's first attempt from the CLI.
    const ref = cli('promote', 'a4')
    const refSibling = cli('show', 'a5').attempt
    // Same action from the extension on t1.
    const a2 = api.tree.allAttempts.find((a) => a.id === 2)!
    const r = (await vscode.commands.executeCommand('agentvc.promote', attemptNode(a2), { confirm: false })) as {
      attempt: Attempt
      closed_siblings: number[]
    }
    assert.equal(r.attempt.status, ref.attempt.status)
    assert.deepEqual(r.closed_siblings, [3])
    assert.deepEqual(ref.closed_siblings, [5])
    const sibling = cli('show', 'a3').attempt
    assert.equal(sibling.status, refSibling.status)
    assert.equal(sibling.reason, 'lost to a2')
    assert.equal(refSibling.reason, 'lost to a4')
    assert.equal(cli('show', 'a2').attempt.status, 'promoted')
    await until('promoted in tree', () => api.tree.allAttempts.find((a) => a.id === 2 && a.status === 'promoted'))
  })

  it('shows the attempt of an open view in the status bar', async () => {
    // Adding a folder would restart the extension host; check the matching directly.
    const a3 = cli('path', 'a3').path as string
    await api.statusBar.update([path.join(a3, 'src')])
    assert.equal(api.statusBar.current?.attempt.id, 3)
    assert.ok(api.statusBar.current?.head, 'shows its last snapshot')
  })
})
