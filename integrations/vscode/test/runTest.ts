// Builds a throwaway agentvc space and runs the extension tests in VS Code
// (downloaded by @vscode/test-electron, or VSCODE_EXECUTABLE if set).

import { execFileSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import * as path from 'node:path'
import { runTests } from '@vscode/test-electron'

// Compiled to out/integrations/vscode/test/.
const ext = path.resolve(__dirname, '../../../..')
const bin = process.env.AGENTVC_BIN_DIR ?? path.resolve(ext, '../../target/debug')

export function makeFixture(): string {
  const dir = mkdtempSync(path.join(tmpdir(), 'agentvc-vscode-'))
  const sh = (cmd: string, args: string[], cwd = dir) =>
    execFileSync(cmd, args, { cwd, encoding: 'utf8', env: { ...process.env, GIT_AUTHOR_NAME: 't', GIT_AUTHOR_EMAIL: 't@t', GIT_COMMITTER_NAME: 't', GIT_COMMITTER_EMAIL: 't@t' } })
  const av = (...args: string[]) => JSON.parse(sh(path.join(bin, 'agentvc'), ['--json', ...args]))
  sh('git', ['init', '-q'])
  writeFileSync(path.join(dir, 'a.txt'), 'one\ntwo\nthree\n')
  writeFileSync(path.join(dir, 'gone.txt'), 'remove me\n')
  writeFileSync(path.join(dir, 'keep.txt'), 'unchanged\n')
  sh('git', ['add', '.'])
  sh('git', ['commit', '-qm', 'init'])
  av('init')
  av('task', 'new', 'Make a.txt better', '--criteria', 'tests pass')
  const fork = (task: string) =>
    av('fork', '--task', task, '--from', 'a1', '--backend', 'copy', '--actor', 'agent:test').fork.attempt
  // t1: a2 adds, removes and changes files; a3 only changes a.txt.
  const a2 = fork('t1')
  writeFileSync(path.join(a2.view_path, 'a.txt'), 'one\nTWO\nthree\n')
  writeFileSync(path.join(a2.view_path, 'new.txt'), 'brand new\n')
  rmSync(path.join(a2.view_path, 'gone.txt'))
  av('snapshot', `a${a2.id}`)
  const a3 = fork('t1')
  writeFileSync(path.join(a3.view_path, 'a.txt'), 'one\n2\nthree\n')
  av('snapshot', `a${a3.id}`)
  av('mark', `a${a2.id}`, 'passed')
  av('mark', `a${a3.id}`, 'passed')
  // t2: promoted from the CLI, to compare with promoting from the extension.
  av('task', 'new', 'Reference task')
  fork('t2')
  fork('t2')

  mkdirSync(path.join(dir, '.vscode'))
  writeFileSync(
    path.join(dir, '.vscode', 'settings.json'),
    JSON.stringify({
      // Nothing listens on port 9: the extension must start its own daemon.
      'agentvc.daemonUrl': 'http://127.0.0.1:9',
      'agentvc.cliPath': path.join(bin, 'agentvc'),
      'agentvc.daemonPath': path.join(bin, 'agentvcd'),
    }),
  )
  return dir
}

async function main() {
  const workspace = makeFixture()
  try {
    await runTests({
      extensionDevelopmentPath: ext,
      extensionTestsPath: path.resolve(__dirname, 'suite/index'),
      vscodeExecutablePath: process.env.VSCODE_EXECUTABLE || undefined,
      launchArgs: [workspace, '--disable-extensions', '--disable-workspace-trust'],
      extensionTestsEnv: { AGENTVC_BIN_DIR: bin, AGENTVC_FIXTURE: workspace },
    })
  } catch (e) {
    console.error('extension tests failed:', e)
    process.exitCode = 1
  } finally {
    rmSync(workspace, { recursive: true, force: true })
  }

  // A plain git repository with no space: the panel must set it up itself.
  const fresh = makeFreshRepo()
  try {
    await runTests({
      extensionDevelopmentPath: ext,
      extensionTestsPath: path.resolve(__dirname, 'suite/index'),
      vscodeExecutablePath: process.env.VSCODE_EXECUTABLE || undefined,
      launchArgs: [fresh, '--disable-extensions', '--disable-workspace-trust'],
      extensionTestsEnv: { AGENTVC_BIN_DIR: bin, AGENTVC_FIXTURE: fresh, AGENTVC_SUITE: 'setup' },
    })
  } catch (e) {
    console.error('set up tests failed:', e)
    process.exitCode = 1
  } finally {
    rmSync(fresh, { recursive: true, force: true })
  }
}

/** A git repository without an agentvc space, set to a CLI that is not there. */
export function makeFreshRepo(): string {
  const dir = mkdtempSync(path.join(tmpdir(), 'agentvc-fresh-'))
  const env = { ...process.env, GIT_AUTHOR_NAME: 't', GIT_AUTHOR_EMAIL: 't@t', GIT_COMMITTER_NAME: 't', GIT_COMMITTER_EMAIL: 't@t' }
  execFileSync('git', ['init', '-q'], { cwd: dir, env })
  writeFileSync(path.join(dir, 'a.txt'), 'one\n')
  execFileSync('git', ['add', '.'], { cwd: dir, env })
  execFileSync('git', ['commit', '-qm', 'init'], { cwd: dir, env })
  mkdirSync(path.join(dir, '.vscode'))
  writeFileSync(
    path.join(dir, '.vscode', 'settings.json'),
    JSON.stringify({
      'agentvc.daemonUrl': 'http://127.0.0.1:9',
      'agentvc.cliPath': path.join(dir, 'missing', 'agentvc'),
      'agentvc.daemonPath': path.join(bin, 'agentvcd'),
    }),
  )
  return dir
}

if (require.main === module) void main()
