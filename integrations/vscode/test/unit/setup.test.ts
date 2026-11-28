import * as assert from 'node:assert/strict'
import { machineLabel, resolveBinary, resolveDaemon } from '../../src/setup'

const fs = (...files: string[]) => (f: string) => files.includes(f)

describe('setup', () => {
  it('finds a bare name on PATH first', () => {
    const exists = fs('/usr/bin/agentvc', '/h/.local/bin/agentvc')
    assert.equal(resolveBinary('agentvc', { PATH: '/bin:/usr/bin', HOME: '/h', platform: 'linux', exists }), '/usr/bin/agentvc')
  })

  it('falls back to ~/.local/bin when PATH misses it (Remote-SSH, dock launches)', () => {
    const exists = fs('/h/.local/bin/agentvc')
    assert.equal(resolveBinary('agentvc', { PATH: '/usr/bin', HOME: '/h', platform: 'linux', exists }), '/h/.local/bin/agentvc')
  })

  it('uses an explicit cliPath as is, expanding ~', () => {
    const exists = fs('/opt/x/agentvc', '/h/bin/agentvc')
    const env = { PATH: '', HOME: '/h', platform: 'linux' as const, exists }
    assert.equal(resolveBinary('/opt/x/agentvc', env), '/opt/x/agentvc')
    assert.equal(resolveBinary('~/bin/agentvc', env), '/h/bin/agentvc')
    assert.equal(resolveBinary('/missing/agentvc', env), undefined)
  })

  it('reports a missing CLI', () => {
    assert.equal(resolveBinary('agentvc', { PATH: '/usr/bin', HOME: '/h', platform: 'linux', exists: fs() }), undefined)
  })

  it('finds agentvcd next to the CLI', () => {
    const exists = fs('/opt/x/agentvc', '/opt/x/agentvcd')
    assert.equal(resolveDaemon('agentvcd', '/opt/x/agentvc', { PATH: '', HOME: '/h', platform: 'linux', exists }), '/opt/x/agentvcd')
  })

  it('names the machine the panel talks about', () => {
    assert.equal(machineLabel(undefined, 'mac'), 'this computer')
    assert.equal(machineLabel('ssh-remote', 'spark'), 'spark')
    assert.equal(machineLabel('dev-container', 'abc123'), 'this container')
  })
})
