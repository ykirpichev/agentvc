// Syntax highlighting for the Code tab, loaded on first use so the rest of
// the UI does not carry it. highlight.js core with the languages people
// keep in repos; anything else shows as plain text.

import type { HLJSApi, LanguageFn } from 'highlight.js'

const BY_EXT: Record<string, string> = {
  rs: 'rust',
  ts: 'typescript',
  tsx: 'typescript',
  mts: 'typescript',
  cts: 'typescript',
  js: 'javascript',
  jsx: 'javascript',
  mjs: 'javascript',
  cjs: 'javascript',
  py: 'python',
  go: 'go',
  java: 'java',
  kt: 'kotlin',
  kts: 'kotlin',
  swift: 'swift',
  c: 'c',
  h: 'c',
  cc: 'cpp',
  cpp: 'cpp',
  cxx: 'cpp',
  hpp: 'cpp',
  hh: 'cpp',
  cs: 'csharp',
  rb: 'ruby',
  php: 'php',
  sh: 'bash',
  bash: 'bash',
  zsh: 'bash',
  ps1: 'powershell',
  json: 'json',
  jsonc: 'json',
  toml: 'ini',
  ini: 'ini',
  cfg: 'ini',
  yaml: 'yaml',
  yml: 'yaml',
  md: 'markdown',
  markdown: 'markdown',
  css: 'css',
  scss: 'scss',
  html: 'xml',
  htm: 'xml',
  xml: 'xml',
  svg: 'xml',
  sql: 'sql',
  proto: 'protobuf',
  lua: 'lua',
  dockerfile: 'dockerfile',
  makefile: 'makefile',
  mk: 'makefile',
  bzl: 'python',
  bazel: 'python',
  star: 'python',
  gradle: 'kotlin',
  scala: 'scala',
  dart: 'dart',
  ex: 'elixir',
  exs: 'elixir',
  hs: 'haskell',
  diff: 'diff',
  patch: 'diff',
}

const BY_NAME: Record<string, string> = {
  dockerfile: 'dockerfile',
  makefile: 'makefile',
  'cargo.lock': 'ini',
  build: 'python',
  workspace: 'python',
  '.gitignore': 'bash',
  '.bashrc': 'bash',
  '.zshrc': 'bash',
}

/** The highlight.js language for a path, or null for plain text. */
export function languageOf(path: string): string | null {
  const name = (path.split('/').pop() ?? '').toLowerCase()
  if (BY_NAME[name]) return BY_NAME[name]!
  if (name.startsWith('dockerfile')) return 'dockerfile'
  const ext = name.includes('.') ? name.split('.').pop()! : ''
  return BY_EXT[ext] ?? null
}

const LOADERS: Record<string, () => Promise<{ default: LanguageFn }>> = {
  rust: () => import('highlight.js/lib/languages/rust'),
  typescript: () => import('highlight.js/lib/languages/typescript'),
  javascript: () => import('highlight.js/lib/languages/javascript'),
  python: () => import('highlight.js/lib/languages/python'),
  go: () => import('highlight.js/lib/languages/go'),
  java: () => import('highlight.js/lib/languages/java'),
  kotlin: () => import('highlight.js/lib/languages/kotlin'),
  swift: () => import('highlight.js/lib/languages/swift'),
  c: () => import('highlight.js/lib/languages/c'),
  cpp: () => import('highlight.js/lib/languages/cpp'),
  csharp: () => import('highlight.js/lib/languages/csharp'),
  ruby: () => import('highlight.js/lib/languages/ruby'),
  php: () => import('highlight.js/lib/languages/php'),
  bash: () => import('highlight.js/lib/languages/bash'),
  powershell: () => import('highlight.js/lib/languages/powershell'),
  json: () => import('highlight.js/lib/languages/json'),
  ini: () => import('highlight.js/lib/languages/ini'),
  yaml: () => import('highlight.js/lib/languages/yaml'),
  markdown: () => import('highlight.js/lib/languages/markdown'),
  css: () => import('highlight.js/lib/languages/css'),
  scss: () => import('highlight.js/lib/languages/scss'),
  xml: () => import('highlight.js/lib/languages/xml'),
  sql: () => import('highlight.js/lib/languages/sql'),
  protobuf: () => import('highlight.js/lib/languages/protobuf'),
  lua: () => import('highlight.js/lib/languages/lua'),
  dockerfile: () => import('highlight.js/lib/languages/dockerfile'),
  makefile: () => import('highlight.js/lib/languages/makefile'),
  scala: () => import('highlight.js/lib/languages/scala'),
  dart: () => import('highlight.js/lib/languages/dart'),
  elixir: () => import('highlight.js/lib/languages/elixir'),
  haskell: () => import('highlight.js/lib/languages/haskell'),
  diff: () => import('highlight.js/lib/languages/diff'),
}

let core: Promise<HLJSApi> | null = null
const loaded = new Set<string>()

async function hljsWith(lang: string): Promise<HLJSApi | null> {
  core ??= import('highlight.js/lib/core').then((m) => m.default)
  const hl = await core
  if (!loaded.has(lang)) {
    const load = LOADERS[lang]
    if (!load) return null
    hl.registerLanguage(lang, (await load()).default)
    loaded.add(lang)
  }
  return hl
}

/** Files bigger than this show as plain text: highlighting them would stall the page. */
export const MAX_HIGHLIGHT_BYTES = 512 * 1024

/**
 * Split highlighted HTML into one string per line, closing the spans open at
 * each line break and reopening them on the next line.
 */
export function splitLines(html: string): string[] {
  const out: string[] = []
  const open: string[] = []
  let line = ''
  const re = /(<span[^>]*>)|(<\/span>)|(\n)/g
  let last = 0
  for (let m = re.exec(html); m; m = re.exec(html)) {
    line += html.slice(last, m.index)
    last = re.lastIndex
    if (m[1]) {
      open.push(m[1])
      line += m[1]
    } else if (m[2]) {
      open.pop()
      line += m[2]
    } else {
      out.push(line + '</span>'.repeat(open.length))
      line = open.join('')
    }
  }
  out.push(line + html.slice(last))
  return out
}

/** Per-line HTML for `text`, or null when the language is unknown or the file too big. */
export async function highlightLines(text: string, path: string): Promise<string[] | null> {
  const lang = languageOf(path)
  if (!lang || text.length > MAX_HIGHLIGHT_BYTES) return null
  const hl = await hljsWith(lang)
  if (!hl) return null
  const html = hl.highlight(text, { language: lang, ignoreIllegals: true }).value
  const lines = splitLines(html)
  // A trailing newline leaves an empty last line the plain view does not show.
  if (text.endsWith('\n') && lines.length > 1 && lines[lines.length - 1] === '') lines.pop()
  return lines
}
