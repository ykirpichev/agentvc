// Finding the agentvc binaries and setting a machine up, kept free of
// `vscode` so it can be unit tested. Editors started from a dock or over
// Remote-SSH often get a PATH without ~/.local/bin, where get.sh installs,
// so a bare name is also looked up in the usual install folders.

import { accessSync, constants, statSync } from 'node:fs'
import * as os from 'node:os'
import * as path from 'node:path'

/** The installer the "Install agentvc" button runs (the release's get.sh). */
export const INSTALL_URL = 'https://raw.githubusercontent.com/ykirpichev/agentvc/main/scripts/get.sh'
export const INSTALL_COMMAND = `curl -fsSL ${INSTALL_URL} | sh`
/** Also installs this extension with the editor CLI found there (in a remote window, on the remote host). */
export const INSTALL_WITH_EXTENSION_COMMAND = `curl -fsSL ${INSTALL_URL} | sh -s -- --vscode`

export type IsExecutable = (file: string) => boolean

export const isExecutable: IsExecutable = (file) => {
  try {
    if (!statSync(file).isFile()) return false
    if (process.platform !== 'win32') accessSync(file, constants.X_OK)
    return true
  } catch {
    return false
  }
}

/** Folders searched after PATH, in order: get.sh's default first. */
export function fallbackDirs(home: string, platform: NodeJS.Platform = process.platform): string[] {
  if (platform === 'win32') {
    return [path.win32.join(home, '.local', 'bin'), path.win32.join(home, '.cargo', 'bin')]
  }
  return [
    path.posix.join(home, '.local', 'bin'),
    path.posix.join(home, '.cargo', 'bin'),
    '/usr/local/bin',
    '/opt/homebrew/bin',
  ]
}

export interface ResolveEnv {
  PATH?: string
  HOME?: string
  platform?: NodeJS.Platform
  exists?: IsExecutable
}

/**
 * The binary to run for `configured` (a setting such as `agentvc.cliPath`):
 * a path (absolute, `~/…` or containing a separator) is used as is when it
 * exists; a bare name is searched on PATH, then in the install folders.
 * Undefined when nothing is found.
 */
export function resolveBinary(configured: string, env: ResolveEnv = {}): string | undefined {
  const platform = env.platform ?? process.platform
  const exists = env.exists ?? isExecutable
  const home = env.HOME ?? os.homedir()
  const p = platform === 'win32' ? path.win32 : path.posix
  let name = configured.trim()
  if (!name) return undefined
  if (name === '~' || name.startsWith('~/')) name = p.join(home, name.slice(1))
  const exts = platform === 'win32' ? ['', '.exe', '.cmd'] : ['']
  const tryAt = (file: string) => exts.map((e) => file + e).find(exists)
  if (p.isAbsolute(name) || name.includes('/') || (platform === 'win32' && name.includes('\\'))) {
    return tryAt(name)
  }
  const pathDirs = (env.PATH ?? process.env.PATH ?? '').split(platform === 'win32' ? ';' : ':').filter(Boolean)
  for (const dir of [...pathDirs, ...fallbackDirs(home, platform)]) {
    const hit = tryAt(p.join(dir, name))
    if (hit) return hit
  }
  return undefined
}

/**
 * The daemon to run: `configured` when it resolves, else `agentvcd` next to
 * the CLI (the release tarball ships both side by side).
 */
export function resolveDaemon(configured: string, cli: string | undefined, env: ResolveEnv = {}): string | undefined {
  const found = resolveBinary(configured, env)
  if (found || !cli) return found
  const p = (env.platform ?? process.platform) === 'win32' ? path.win32 : path.posix
  return resolveBinary(p.join(p.dirname(cli), 'agentvcd'), env)
}

/** The host of a remote folder URI's authority: `ssh-remote+spark` -> `spark`. */
export function remoteHost(authority: string): string {
  const i = authority.indexOf('+')
  const host = i >= 0 ? authority.slice(i + 1) : authority
  // Some hosts are hex-encoded JSON ({"hostName":"spark"}).
  if (/^[0-9a-f]+$/i.test(host) && host.length % 2 === 0 && host.length > 8) {
    try {
      const o = JSON.parse(Buffer.from(host, 'hex').toString('utf8')) as { hostName?: string }
      if (o.hostName) return o.hostName
    } catch {
      /* not JSON */
    }
  }
  return host
}

/** How the panel names the machine: "spark" for SSH, else "this computer". */
export function machineLabel(remoteName: string | undefined, hostname: string): string {
  if (!remoteName) return 'this computer'
  if (remoteName === 'ssh-remote') return hostname || 'the remote host'
  if (remoteName === 'wsl') return 'WSL'
  if (remoteName === 'dev-container' || remoteName === 'attached-container') return 'this container'
  if (remoteName === 'codespaces') return 'this codespace'
  return hostname || 'the remote host'
}
