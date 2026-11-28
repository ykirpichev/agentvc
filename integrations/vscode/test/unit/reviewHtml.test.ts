import * as assert from 'node:assert/strict'
import { THEME_VARS, WEBVIEW_COMMANDS, frameHtml, offlineHtml, originOf } from '../../src/reviewHtml'

describe('reviewHtml', () => {
  it('frames the daemon UI with a CSP limited to its origin', () => {
    const html = frameHtml({
      base: 'http://127.0.0.1:7420/',
      path: '/review/v/3',
      surface: 'sidebar',
      folder: '/repo/wt',
      cspSource: 'vscode-resource:',
      nonce: 'N0NCE',
    })
    assert.match(html, /frame-src http:\/\/127\.0\.0\.1:7420;/)
    assert.match(html, /script-src 'nonce-N0NCE'/)
    const cfg = JSON.parse(/<script nonce="N0NCE" type="application\/json" id="cfg">(.*?)<\/script>/s.exec(html)![1])
    assert.deepEqual(cfg, {
      base: 'http://127.0.0.1:7420',
      path: '/review/v/3',
      surface: 'sidebar',
      folder: '/repo/wt',
      origin: 'http://127.0.0.1:7420',
      vars: [...THEME_VARS],
    })
    // The editor's theme (controls and fonts) follows the frame, on load and on change.
    assert.match(html, /addEventListener\('load', sendTheme\)/)
    assert.match(html, /d\.type === 'agentvc\.ready'\) sendTheme\(\)/)
    assert.ok(THEME_VARS.includes('button-background') && THEME_VARS.includes('dropdown-background'))
  })

  it('cannot be broken out of by a folder name', () => {
    const html = frameHtml({ base: 'http://localhost:1', path: '/review', surface: 'panel', folder: '/x/</script><b>', cspSource: 'c', nonce: 'n' })
    assert.ok(!html.includes('</script><b>'))
  })

  it('offers to start the daemon when offline, and only known commands', () => {
    const html = offlineHtml({ state: 'offline', cspSource: 'c', nonce: 'n' })
    assert.match(html, /data-cmd="agentvc.startDaemon"/)
    assert.ok(!offlineHtml({ state: 'connecting', cspSource: 'c', nonce: 'n' }).includes('data-cmd="'))
    assert.deepEqual([...WEBVIEW_COMMANDS], ['agentvc.startDaemon', 'agentvc.refresh', 'agentvc.install', 'agentvc.setup'])
  })

  it('offers to install agentvc on the remote host when the CLI is missing', () => {
    const html = offlineHtml({ state: 'nocli', machine: 'spark', folder: '/w', cspSource: 'c', nonce: 'n' })
    assert.match(html, /not installed on spark/)
    assert.match(html, /data-cmd="agentvc.install">Install agentvc on spark</)
    assert.ok(!html.includes('agentvc.startDaemon'))
  })

  it('explains a local copy of the extension in a remote window', () => {
    const html = offlineHtml({ state: 'localonly', machine: 'spark', folder: '/w', cspSource: 'c', nonce: 'n' })
    assert.match(html, /runs on your computer/)
    assert.match(html, /data-cmd="agentvc.install">Install on spark</)
  })

  it('offers one-click set up when the folder has no space, not agentvc init', () => {
    const html = offlineHtml({ state: 'nospace', machine: 'spark', folder: '/w', cspSource: 'c', nonce: 'n' })
    assert.match(html, /data-cmd="agentvc.setup">Set up agentvc here</)
    assert.ok(!html.includes('Run <code>agentvc init'))
    assert.ok(!offlineHtml({ state: 'nospace', cspSource: 'c', nonce: 'n' }).includes('agentvc.setup'))
    assert.ok(!offlineHtml({ state: 'nocli', machine: '<b>', cspSource: 'c', nonce: 'n' }).includes('<b>'))
  })

  it('takes the origin of a forwarded URL', () => {
    assert.equal(originOf('https://abc-7420.app.github.dev/'), 'https://abc-7420.app.github.dev')
  })
})
