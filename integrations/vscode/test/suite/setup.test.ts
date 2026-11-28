// Runs in a git repository with no agentvc space (AGENTVC_SUITE=setup):
// the panel says what is missing and fixes it with one click.

import * as assert from 'node:assert/strict'
import { existsSync } from 'node:fs'
import * as path from 'node:path'
import * as vscode from 'vscode'
import type { AgentvcApi } from '../../src/extension'

const bin = process.env.AGENTVC_BIN_DIR!
const repo = process.env.AGENTVC_FIXTURE!

async function until<T>(what: string, fn: () => T | undefined | Promise<T | undefined>, ms = 15_000): Promise<T> {
  const end = Date.now() + ms
  for (;;) {
    const v = await fn()
    if (v) return v
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`)
    await new Promise((r) => setTimeout(r, 100))
  }
}

describe('setting agentvc up from the panel', function () {
  let api: AgentvcApi

  before(async () => {
    api = await vscode.extensions.getExtension<AgentvcApi>('avc.agentvc')!.activate()
  })

  it('says the CLI is missing instead of asking for agentvc init', async () => {
    await until('nocli', () => api.connection.state === 'nocli')
  })

  it('offers set up once the CLI is found, and sets the folder up in one click', async () => {
    const cfg = vscode.workspace.getConfiguration('agentvc')
    await cfg.update('cliPath', path.join(bin, 'agentvc'), vscode.ConfigurationTarget.Workspace)
    await until('nospace', () => api.connection.state === 'nospace')
    assert.ok(!existsSync(path.join(repo, '.agentvc')))
    const ok = await vscode.commands.executeCommand<boolean>('agentvc.setup')
    assert.equal(ok, true)
    assert.ok(existsSync(path.join(repo, '.agentvc')), 'space created')
    await until('connected', () => api.connection.state === 'connected')
    assert.ok(api.connection.daemon, 'started its own agentvcd')
  })
})
