import { readdirSync } from 'node:fs'
import * as path from 'node:path'
import Mocha from 'mocha'

export function run(): Promise<void> {
  const mocha = new Mocha({ ui: 'bdd', color: true, timeout: 30_000 })
  // AGENTVC_SUITE picks one file (its own workspace, see runTest.ts); the
  // default run takes every other one.
  const only = process.env.AGENTVC_SUITE
  for (const f of readdirSync(__dirname)) {
    if (!f.endsWith('.test.js')) continue
    if (only ? f !== `${only}.test.js` : f === 'setup.test.js') continue
    mocha.addFile(path.join(__dirname, f))
  }
  return new Promise((resolve, reject) => {
    mocha.run((failures) => (failures ? reject(new Error(`${failures} tests failed`)) : resolve()))
  })
}
