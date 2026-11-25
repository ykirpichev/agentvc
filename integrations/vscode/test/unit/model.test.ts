import * as assert from 'node:assert/strict'
import { attemptForPath, attemptForest, countsLabel, diffEntries, parseRef, snapshotBase } from '../../src/model'
import type { Attempt, Snapshot } from '../../src/types'

function attempt(id: number, parent: number | null, extra: Partial<Attempt> = {}): Attempt {
  return {
    id,
    task_id: 1,
    parent_attempt: parent,
    parent_snapshot: parent === null ? null : parent * 10,
    actor: 'agent',
    model: null,
    prompt: null,
    status: 'exploring',
    reason: null,
    view_path: `/repo/.agentvc/views/a${id}`,
    backend: 'copy',
    created_at: 0,
    ...extra,
  }
}

describe('model', () => {
  it('nests attempts under the attempt they forked from', () => {
    const forest = attemptForest([attempt(4, 2), attempt(2, 1), attempt(3, 1), attempt(1, null)])
    assert.equal(forest.length, 1)
    assert.deepEqual(forest[0].children.map((c) => c.attempt.id), [2, 3])
    assert.deepEqual(forest[0].children[0].children.map((c) => c.attempt.id), [4])
  })

  it('keeps attempts whose parent is in another task at the top level', () => {
    const forest = attemptForest([attempt(5, 1), attempt(6, 5)])
    assert.deepEqual(forest.map((n) => n.attempt.id), [5])
  })

  it('labels status counts in review order', () => {
    const c = { total: 4, exploring: 1, passed: 2, failed: 0, abandoned: 1, promoted: 0 }
    assert.equal(countsLabel(c), '2 passed · 1 exploring · 1 abandoned')
    assert.equal(countsLabel({ ...c, total: 0, exploring: 0, passed: 0, abandoned: 0 }), 'no attempts')
  })

  it('puts added files only on the right and removed files only on the left', () => {
    const e = diffEntries([
      { path: 'b.txt', kind: 'removed' },
      { path: 'a.txt', kind: 'added' },
      { path: 'c.txt', kind: 'modified' },
    ])
    assert.deepEqual(
      e.map((x) => [x.path, x.left, x.right]),
      [
        ['a.txt', false, true],
        ['b.txt', true, false],
        ['c.txt', true, true],
      ],
    )
  })

  it('finds the attempt whose view holds a folder', () => {
    const all = [attempt(2, 1), attempt(12, 1)]
    assert.equal(attemptForPath('/repo/.agentvc/views/a12/src', all)?.id, 12)
    assert.equal(attemptForPath('/repo/.agentvc/views/a2', all)?.id, 2)
    assert.equal(attemptForPath('/repo/.agentvc/views/a1', all), undefined)
  })

  it('diffs a snapshot against the previous one, else the fork point', () => {
    const a = attempt(3, 1)
    const snap = (id: number): Snapshot => ({ id, attempt_id: 3, tree: '', trigger: 't', meta: null, created_at: 0 })
    const all = [snap(11), snap(15), snap(20)]
    assert.equal(snapshotBase(all[2], all, a), 's15')
    assert.equal(snapshotBase(all[0], all, a), 's10')
  })

  it('parses refs', () => {
    assert.equal(parseRef('a3'), 'a3')
    assert.equal(parseRef('s12'), 's12')
    assert.equal(parseRef('7'), 'a7')
    assert.equal(parseRef('t1'), undefined)
  })
})
