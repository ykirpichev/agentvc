import * as assert from 'node:assert/strict'
import {
  branchDescription,
  canMoveOnto,
  githubRepo,
  position,
  stackForest,
  staleCount,
  type StackBranch,
  type StackLog,
} from '../../src/stackModel'

function b(name: string, parent: string, extra: Partial<StackBranch> = {}): StackBranch {
  return {
    name,
    parent,
    parent_kind: parent === 'main' ? 'trunk' : 'stack',
    parent_rev: 'r',
    tip: 't',
    commits: 1,
    needs_restack: false,
    current: false,
    depth: 0,
    children: [],
    ...extra,
  }
}

function log(branches: StackBranch[]): StackLog {
  const byName = new Map(branches.map((x) => [x.name, x]))
  for (const x of branches) {
    const p = byName.get(x.parent)
    if (p && x.parent_kind === 'stack') p.children.push(x.name)
  }
  return {
    window: '/w1',
    git_dir: '/repo/.git/worktrees/w1',
    trunk: 'main',
    base_ref: 'origin/main',
    base: 'abc',
    current: branches.find((x) => x.current)?.name ?? null,
    branches,
  }
}

describe('stack model', () => {
  const chain = () =>
    log([
      b('s1', 'main'),
      b('s2', 's1', { depth: 1, current: true, pr: 12 }),
      b('s3', 's2', { depth: 2, needs_restack: true }),
      b('side', 'main', { parent_kind: 'other_window', parent: 'theirs' }),
    ])

  it('nests branches under their parents', () => {
    const f = stackForest(chain())
    assert.deepEqual(f.map((n) => n.branch.name), ['s1', 'side'])
    assert.equal(f[0].children[0].branch.name, 's2')
    assert.equal(f[0].children[0].children[0].branch.name, 's3')
  })

  it('describes a branch and where it sits', () => {
    const l = chain()
    assert.equal(branchDescription(l.branches[0], l), '1 commit · on origin/main')
    assert.equal(branchDescription(l.branches[1], l), '1 commit · #12')
    assert.equal(branchDescription(l.branches[2], l), '1 commit · needs restack')
    assert.equal(branchDescription(l.branches[3], l), '1 commit · on theirs (other window)')
    assert.deepEqual(position(l), { branch: 's2', index: 2, total: 3 })
    assert.equal(staleCount(l), 1)
  })

  it('refuses to move a branch onto itself or what is stacked on it', () => {
    const l = chain()
    assert.equal(canMoveOnto(l, 's2', 's3'), false)
    assert.equal(canMoveOnto(l, 's2', 's2'), false)
    assert.equal(canMoveOnto(l, 's3', 's1'), true)
    assert.equal(canMoveOnto(l, 's2', 'theirs'), true)
  })

  it('reads the GitHub repository from remote URLs', () => {
    assert.equal(githubRepo('git@github.com:o/r.git'), 'o/r')
    assert.equal(githubRepo('https://github.com/o/r'), 'o/r')
    assert.equal(githubRepo('http://proxy/git/o/r'), undefined)
  })
})
