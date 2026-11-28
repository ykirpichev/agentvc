// The HTML of the editor's review surfaces: a frame holding the daemon's own
// review UI (the same React app as the web UI), so VS Code, Cursor and the
// browser show the same screen. Kept free of `vscode` so it can be unit tested.

export type Surface = 'sidebar' | 'panel'

/** Commands a webview may run; nothing else it sends is acted on. */
export const WEBVIEW_COMMANDS = ['agentvc.startDaemon', 'agentvc.refresh', 'agentvc.install', 'agentvc.setup'] as const

export function nonce(): string {
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789'
  let s = ''
  for (let i = 0; i < 32; i++) s += chars[Math.floor(Math.random() * chars.length)]
  return s
}

const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!)

/** The origin (scheme://host:port) of a URL, for CSP and postMessage. */
export function originOf(url: string): string {
  const u = new URL(url)
  return `${u.protocol}//${u.host}`
}

/**
 * A webview page that frames `base + path` with `?embed=<surface>`, the
 * editor's theme and the window's folder, and relays messages both ways:
 * the frame's `agentvc.*` messages go to the extension, the extension's
 * `agentvc.navigate` goes to the frame, and theme changes follow the editor.
 */
export function frameHtml(o: { base: string; path: string; surface: Surface; folder?: string; cspSource: string; nonce: string }): string {
  const origin = originOf(o.base)
  const cfg = JSON.stringify({ base: o.base.replace(/\/+$/, ''), path: o.path, surface: o.surface, folder: o.folder ?? '', origin })
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; frame-src ${esc(origin)}; style-src ${o.cspSource} 'unsafe-inline'; script-src 'nonce-${o.nonce}';">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  html, body { margin: 0; padding: 0; height: 100%; overflow: hidden; background: transparent; }
  iframe { border: 0; width: 100%; height: 100vh; display: block; background: transparent; }
</style>
</head>
<body>
<iframe id="f" title="agentvc review" allow="clipboard-read; clipboard-write"></iframe>
<script nonce="${o.nonce}" type="application/json" id="cfg">${cfg.replace(/</g, '\\u003c')}</script>
<script nonce="${o.nonce}">
(function () {
  const vscode = acquireVsCodeApi();
  const cfg = JSON.parse(document.getElementById('cfg').textContent);
  const frame = document.getElementById('f');
  const theme = () => /vscode-light|vscode-high-contrast-light/.test(document.body.className) ? 'light' : 'dark';
  const bg = () => {
    const v = cfg.surface === 'sidebar' ? '--vscode-sideBar-background' : '--vscode-editor-background';
    return getComputedStyle(document.documentElement).getPropertyValue(v).trim();
  };
  const q = new URLSearchParams({ embed: cfg.surface, theme: theme() });
  if (bg()) q.set('bg', bg());
  if (cfg.folder) q.set('here', cfg.folder);
  const sep = cfg.path.includes('?') ? '&' : '?';
  frame.src = cfg.base + cfg.path + sep + q.toString();
  const toFrame = (m) => frame.contentWindow && frame.contentWindow.postMessage(m, cfg.origin);
  // VS Code swaps the body class and the html style's color variables on a theme change.
  const follow = new MutationObserver(() => toFrame({ type: 'agentvc.theme', theme: theme(), bg: bg() }));
  follow.observe(document.body, { attributes: true, attributeFilter: ['class'] });
  follow.observe(document.documentElement, { attributes: true, attributeFilter: ['style'] });
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || typeof d.type !== 'string' || !d.type.startsWith('agentvc.')) return;
    if (e.source === frame.contentWindow) {
      if (e.origin === cfg.origin) vscode.postMessage(d);
      return;
    }
    if (d.type === 'agentvc.navigate') toFrame(d);
  });
})();
</script>
</body>
</html>`
}

/** What the panel shows instead of the frame, by connection state. */
export function offlineContent(o: { state: string; machine?: string; folder?: string }): { text: string; buttons: string } {
  const on = o.machine ?? 'this computer'
  const retry = `<button data-cmd="agentvc.refresh" class="sec">Retry</button>`
  switch (o.state) {
    case 'connecting':
      return { text: 'Connecting to agentvcd…', buttons: '' }
    case 'localonly':
      return {
        text: `This window's folder is on ${esc(on)}, but this copy of the agentvc extension runs on your computer, so it cannot see the folder. Install agentvc and the extension on ${esc(on)}: the button runs <code>get.sh --vscode</code> in this window's terminal (which runs on ${esc(on)}). Then reload the window.`,
        buttons: `<button data-cmd="agentvc.install">Install on ${esc(on)}</button> ${retry}`,
      }
    case 'nocli':
      return {
        text: `agentvc is not installed on ${esc(on)}. Install puts <code>agentvc</code> and <code>agentvcd</code> in <code>~/.local/bin</code>, then sets up this folder.`,
        buttons: `<button data-cmd="agentvc.install">Install agentvc on ${esc(on)}</button> ${retry}`,
      }
    case 'nospace':
      if (!o.folder) return { text: 'Open a folder to use agentvc.', buttons: '' }
      return {
        text: `This folder is not set up for agentvc yet. Set up creates its space in <code>.agentvc</code> (ignored by git) and starts agentvcd on ${esc(on)}.`,
        buttons: `<button data-cmd="agentvc.setup">Set up agentvc here</button> ${retry}`,
      }
    default:
      return {
        text: `The branch graph and review need agentvcd running for this workspace on ${esc(on)}.`,
        buttons: `<button data-cmd="agentvc.startDaemon">Start agentvcd</button> ${retry}`,
      }
  }
}

/**
 * Shown instead of the frame when no daemon is reachable: what is missing on
 * which machine, and a button that fixes it (install, set up, start).
 */
export function offlineHtml(o: { state: string; machine?: string; folder?: string; cspSource: string; nonce: string }): string {
  const { text, buttons } = offlineContent(o)
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${o.cspSource} 'unsafe-inline'; script-src 'nonce-${o.nonce}';">
<style>
  body { font-family: var(--vscode-font-family); font-size: var(--vscode-font-size); color: var(--vscode-foreground); padding: 8px 12px; }
  button { font: inherit; color: var(--vscode-button-foreground); background: var(--vscode-button-background); border: 0; padding: 4px 10px; margin: 6px 6px 0 0; cursor: pointer; border-radius: 2px; }
  button.sec { color: var(--vscode-button-secondaryForeground); background: var(--vscode-button-secondaryBackground); }
</style>
</head>
<body>
<p>${text}</p>
${buttons}
<script nonce="${o.nonce}">
  const vscode = acquireVsCodeApi();
  document.querySelectorAll('button[data-cmd]').forEach((b) =>
    b.addEventListener('click', () => vscode.postMessage({ type: 'agentvc.command', command: b.dataset.cmd })));
</script>
</body>
</html>`
}
