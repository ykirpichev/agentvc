// The Code tab (#172): the whole code of a branch, agent, workspace or
// version. `/review/code/<ref>/<path>#L10-L20`. File tree with the ref's
// changes marked, a file view with highlighting, Diff and Blame for the same
// file, go-to-file (Cmd/Ctrl+P), search (Cmd/Ctrl+Shift+F), a ref switcher
// that keeps the path, and Open in editor.

import { memo, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { Link, useLocation, useNavigate, useParams } from 'react-router'
import { api } from '../api/client'
import type { ChangeKind, FileChange, Ref, SearchMatch, TreeEntry, TreeResponse } from '../api/types'
import { useResource } from '../api/useResource'
import { BlameView } from '../components/BlameView'
import { ErrorNote, Kbd, Skeleton } from '../components/bits'
import { DiffView } from '../components/DiffView'
import { Icon } from '../components/Icon'
import { EDITOR_NAMES, inEditor, joinPath, openInEditor, preferredEditor, setPreferredEditor, type Editor } from '../core/editor'
import { agentLink } from '../core/agents'
import { branchLink, codeLink, versionLink } from '../core/links'
import { useHotkeys } from '../keyboard/hotkeys'
import { bytes, plural } from '../lib/format'
import { fuzzyFilter } from '../lib/fuzzy'
import { highlightLines } from '../lib/highlight'
import { useReview } from './reviewContext'
import './code.css'

/** Rows per chunk; chunks off screen skip layout and paint. */
const CHUNK = 250

type View = 'code' | 'diff' | 'blame'

/** `#L10` or `#L10-L20`. */
function parseLines(hash: string): [number, number] | null {
  const m = /^#L(\d+)(?:-L?(\d+))?$/.exec(hash)
  if (!m) return null
  const a = Number(m[1])
  const b = m[2] ? Number(m[2]) : a
  return [Math.min(a, b), Math.max(a, b)]
}

const parentOf = (p: string) => (p.includes('/') ? p.slice(0, p.lastIndexOf('/')) : '')

/** Where the ref's own page is, for the way back. */
function homeOf(at: string, tree?: TreeResponse): { to: string; label: string } | null {
  let m = /^branch:([^/]+)\/(.+)$/.exec(at)
  if (m) return { to: branchLink(m[1]!, m[2]!), label: m[2]! }
  m = /^agent:(.+)$/.exec(at)
  if (m) return { to: agentLink(m[1]!), label: m[1]! }
  if (tree?.attempt) return { to: versionLink(tree.attempt), label: `a${tree.attempt}` }
  return null
}

function kindWords(t: TreeResponse): string {
  if (t.kind === 'worktree') return t.ref.startsWith('agent:') ? 'live, with edits not committed yet' : 'working tree, with edits not committed yet'
  if (t.kind === 'snapshot') return 'saved version'
  return t.commit ? `at ${t.commit.slice(0, 7)}` : ''
}

export function CodePage() {
  const params = useParams()
  const at = decodeURIComponent(params.ref ?? '')
  const path = (params['*'] ?? '').replace(/\/+$/, '')
  const loc = useLocation()
  const navigate = useNavigate()
  const query = new URLSearchParams(loc.search)
  const view = (query.get('view') as View | null) ?? 'code'
  const searchQ = query.get('q')
  const lines = parseLines(loc.hash)

  const root = useResource(`tree:${at}:`, () => api.tree(at, ''))
  const parent = useResource(path ? `tree:${at}:${parentOf(path)}` : null, () => api.tree(at, parentOf(path)))
  const [goto, setGoto] = useState(false)
  const [changedOnly, setChangedOnly] = useState(false)
  const searchInput = useRef<HTMLInputElement>(null)

  const name = path.split('/').pop() ?? ''
  const entry = path ? parent.data?.entries?.find((e) => e.name === name) : undefined
  const isDir = !path || entry?.kind === 'dir'
  const change = useMemo(() => root.data?.changes?.find((c) => c.path === path)?.kind, [root.data, path])

  const go = (p: string, opts: { lines?: [number, number?]; view?: View; q?: string | null; ref?: string } = {}) => {
    const base = codeLink(opts.ref ?? at, p, opts.lines)
    const qs = new URLSearchParams()
    if (opts.view && opts.view !== 'code') qs.set('view', opts.view)
    if (opts.q) qs.set('q', opts.q)
    const [pathPart, hash] = base.split('#')
    navigate(`${pathPart}${qs.size ? `?${qs}` : ''}${hash ? `#${hash}` : ''}`)
  }

  useHotkeys([
    { keys: ['mod+p', 't'], description: 'Go to file', group: 'Code', handler: () => setGoto(true) },
    {
      keys: ['mod+shift+f'],
      description: 'Search the code',
      group: 'Code',
      inInputs: true,
      handler: () => {
        if (searchQ === null) go(path, { q: '' })
        setTimeout(() => searchInput.current?.focus(), 0)
      },
    },
  ])

  const t = root.data
  const home = homeOf(at, t)
  return (
    <div className="page page-wide code-page">
      <header className="code-head">
        <div className="code-title">
          {home && (
            <Link className="code-back" to={home.to} title={`Back to ${home.label}`}>
              <Icon name="back" size={14} /> {home.label}
            </Link>
          )}
          <h1 className="rv-title">
            Code <span className="muted">·</span> <span className="mono">{t?.label ?? at}</span>
          </h1>
          {t && <span className="muted code-kind">{kindWords(t)}</span>}
        </div>
        <div className="code-tools">
          <RefSwitcher at={at} tree={t} onPick={(r) => go(path, { ref: r, view, lines: lines ?? undefined })} />
          <button type="button" className="btn btn-ghost" onClick={() => setGoto(true)} title="Go to file">
            <Icon name="file" size={14} /> Go to file <Kbd keys="mod+p" />
          </button>
          <button
            type="button"
            className="btn btn-ghost"
            aria-pressed={searchQ !== null}
            onClick={() => (searchQ !== null ? go(path, { view, lines: lines ?? undefined }) : go(path, { q: '' }))}
            title="Search the code"
          >
            <Icon name="search" size={14} /> Search <Kbd keys="mod+shift+f" />
          </button>
        </div>
      </header>
      {root.error && <ErrorNote error={root.error} />}
      {!t && !root.error && <Skeleton rows={6} />}
      {t && (
        <div className="code-body">
          <aside className="code-tree" aria-label="Files">
            <div className="code-tree-head">
              <span className="muted">{t.changed_base ? plural(t.changes?.length ?? 0, 'changed file') : 'Files'}</span>
              {t.changed_base && (t.changes?.length ?? 0) > 0 && (
                <label className="code-toggle">
                  <input type="checkbox" checked={changedOnly} onChange={(e) => setChangedOnly(e.target.checked)} /> Changed only
                </label>
              )}
            </div>
            {changedOnly ? (
              <ChangedList changes={t.changes ?? []} current={path} onOpen={(p) => go(p)} />
            ) : (
              <TreeDir at={at} dir="" depth={0} current={path} onOpen={(p) => go(p)} />
            )}
          </aside>
          <main className="code-main">
            {searchQ !== null ? (
              <SearchPanel at={at} q={searchQ} inputRef={searchInput} onQuery={(q) => go(path, { q })} onOpen={(p, l) => go(p, { lines: [l], q: searchQ })} />
            ) : (
              <>
                <Crumbs at={at} path={path} label={t.label} />
                {isDir ? (
                  <DirListing at={at} dir={path} onOpen={(p) => go(p)} />
                ) : (
                  <FileView
                    at={at}
                    path={path}
                    tree={t}
                    change={change ?? entry?.change}
                    view={view}
                    lines={lines}
                    onView={(v) => go(path, { view: v, lines: lines ?? undefined })}
                    onLines={(l) => go(path, { view, lines: l ?? undefined })}
                  />
                )}
              </>
            )}
          </main>
        </div>
      )}
      {goto && <GoToFile at={at} onClose={() => setGoto(false)} onOpen={(p) => go(p)} />}
    </div>
  )
}

// ---- ref switcher -------------------------------------------------------------

function RefSwitcher({ at, tree, onPick }: { at: string; tree?: TreeResponse; onPick: (r: string) => void }) {
  const list = useResource('workspaces', api.workspaces)
  const agents = useReview()?.agents?.agents ?? []
  const m = /^branch:([^/]+)\/(.+)$/.exec(at)
  const detail = useResource(m ? `branch:${m[1]}:${m[2]}:code` : null, () => api.branch(m![1]!, m![2]!))
  const groups: { label: string; items: { value: string; label: string }[] }[] = []
  const here = { label: 'This', items: [{ value: at, label: tree?.label ?? at }] }
  if (tree?.base && tree.base !== at) here.items.push({ value: tree.base, label: `its base (${tree.base.replace(/^git:/, '').slice(0, 7)})` })
  if (at !== 'main') here.items.push({ value: 'main', label: 'main' })
  groups.push(here)
  for (const w of list.data?.workspaces ?? []) {
    if (w.kind === 'remote') continue
    const items = [{ value: `workspace:${w.id}`, label: `${w.name} · working tree` }]
    for (const b of w.branches) items.push({ value: `branch:${w.id}/${b.name}`, label: b.name })
    groups.push({ label: `Workspace ${w.name}`, items })
  }
  const live = agents.filter((a) => !a.landed && a.state !== 'discarded')
  if (live.length) groups.push({ label: 'Agents', items: live.map((a) => ({ value: `agent:${a.id}`, label: `${a.id} · ${a.title}` })) })
  const older = (detail.data?.commits ?? []).slice(1)
  if (older.length)
    groups.push({ label: 'Older versions of this branch', items: older.map((c) => ({ value: `git:${c.commit}`, label: `${c.commit.slice(0, 7)} ${c.subject}` })) })
  const seen = new Set<string>()
  return (
    <label className="code-ref">
      <Icon name="branch" size={14} />
      <select id="code-ref" value={at} onChange={(e) => onPick(e.target.value)} aria-label="Show the code of">
        {groups.map((g) => (
          <optgroup key={g.label} label={g.label}>
            {g.items
              .filter((i) => !seen.has(i.value) && seen.add(i.value))
              .map((i) => (
                <option key={i.value} value={i.value}>
                  {i.label}
                </option>
              ))}
          </optgroup>
        ))}
      </select>
    </label>
  )
}

// ---- tree ---------------------------------------------------------------------

function ChangeDot({ kind, count }: { kind?: ChangeKind; count?: number }) {
  if (!kind) return null
  return (
    <span className={`code-dot code-dot-${kind}`} title={count ? `${plural(count, 'changed file')} inside` : kind}>
      {count ? count : kind === 'added' ? 'A' : kind === 'removed' ? 'D' : 'M'}
    </span>
  )
}

function TreeDir({ at, dir, depth, current, onOpen }: { at: string; dir: string; depth: number; current: string; onOpen: (p: string) => void }) {
  const data = useResource(`tree:${at}:${dir}`, () => api.tree(at, dir))
  if (data.error) return <div className="code-tree-err muted">Could not list {dir || 'the root'}: {data.error.message}</div>
  if (!data.data) return <div className="code-tree-loading muted" style={{ paddingLeft: 12 + depth * 14 }}>…</div>
  return (
    <ul className="code-tree-list" role={depth === 0 ? 'tree' : 'group'}>
      {(data.data.entries ?? []).map((e) => (
        <TreeItem key={e.path} at={at} e={e} depth={depth} current={current} onOpen={onOpen} />
      ))}
    </ul>
  )
}

function TreeItem({ at, e, depth, current, onOpen }: { at: string; e: TreeEntry; depth: number; current: string; onOpen: (p: string) => void }) {
  const inside = current === e.path || current.startsWith(`${e.path}/`)
  const [open, setOpen] = useState(inside)
  // Opening a file inside (go-to-file, a link) expands the folders on its way.
  const [wasInside, setWasInside] = useState(inside)
  if (inside !== wasInside) {
    setWasInside(inside)
    if (inside) setOpen(true)
  }
  const dir = e.kind === 'dir'
  const removed = e.change === 'removed'
  return (
    <li role="treeitem" aria-expanded={dir ? open : undefined} aria-selected={current === e.path}>
      <button
        type="button"
        className={`code-tree-row${current === e.path ? ' is-current' : ''}${removed ? ' is-removed' : ''}`}
        style={{ paddingLeft: 8 + depth * 14 }}
        onClick={() => {
          if (dir) setOpen((o) => !o)
          if (!dir || !open) onOpen(e.path)
        }}
        title={e.path}
      >
        <span className={`code-caret${dir ? '' : ' is-leaf'}${open ? ' is-open' : ''}`}>{dir ? <Icon name="chevron" size={12} /> : null}</span>
        <Icon name={dir ? 'folder' : 'file'} size={14} className="code-ic" />
        <span className="code-name">{e.name}</span>
        <ChangeDot kind={e.change} count={dir ? e.changed : undefined} />
      </button>
      {dir && open && !removed && <TreeDir at={at} dir={e.path} depth={depth + 1} current={current} onOpen={onOpen} />}
    </li>
  )
}

function ChangedList({ changes, current, onOpen }: { changes: FileChange[]; current: string; onOpen: (p: string) => void }) {
  return (
    <ul className="code-tree-list code-changed">
      {changes.map((c) => (
        <li key={c.path}>
          <button
            type="button"
            className={`code-tree-row${current === c.path ? ' is-current' : ''}${c.kind === 'removed' ? ' is-removed' : ''}`}
            onClick={() => onOpen(c.path)}
            title={c.path}
          >
            <Icon name="file" size={14} className="code-ic" />
            <span className="code-name">
              <span className="code-dir">{parentOf(c.path) ? `${parentOf(c.path)}/` : ''}</span>
              {c.path.split('/').pop()}
            </span>
            <ChangeDot kind={c.kind} />
          </button>
        </li>
      ))}
    </ul>
  )
}

// ---- breadcrumbs and directory --------------------------------------------------

function Crumbs({ at, path, label }: { at: string; path: string; label: string }) {
  const parts = path ? path.split('/') : []
  return (
    <nav className="code-crumbs mono" aria-label="Path">
      <Link to={codeLink(at)}>{label}</Link>
      {parts.map((p, i) => {
        const to = parts.slice(0, i + 1).join('/')
        return (
          <span key={to}>
            <span className="muted"> / </span>
            {i === parts.length - 1 ? <b>{p}</b> : <Link to={codeLink(at, to)}>{p}</Link>}
          </span>
        )
      })}
    </nav>
  )
}

function DirListing({ at, dir, onOpen }: { at: string; dir: string; onOpen: (p: string) => void }) {
  const data = useResource(`tree:${at}:${dir}`, () => api.tree(at, dir))
  if (data.error) return <ErrorNote error={data.error} />
  if (!data.data) return <Skeleton rows={6} />
  const entries = data.data.entries ?? []
  return (
    <section className="code-dir-list">
      {entries.length === 0 && <p className="muted">Empty folder.</p>}
      {entries.map((e) => (
        <button key={e.path} type="button" className={`code-dir-row${e.change === 'removed' ? ' is-removed' : ''}`} onClick={() => onOpen(e.path)}>
          <Icon name={e.kind === 'dir' ? 'folder' : 'file'} size={14} className="code-ic" />
          <span className="mono">{e.name}</span>
          <ChangeDot kind={e.change} count={e.kind === 'dir' ? e.changed : undefined} />
        </button>
      ))}
    </section>
  )
}

// ---- file view -------------------------------------------------------------------

function FileView({
  at,
  path,
  tree,
  change,
  view,
  lines,
  onView,
  onLines,
}: {
  at: string
  path: string
  tree: TreeResponse
  change?: ChangeKind
  view: View
  lines: [number, number] | null
  onView: (v: View) => void
  onLines: (l: [number, number] | null) => void
}) {
  const removed = change === 'removed'
  const file = useResource(removed ? null : `file:${at}:${path}`, () => api.file(at as Ref, path))
  const [wrap, setWrap] = useState(false)
  const base = tree.changed_base
  const canDiff = !!base && !!change
  const f = file.data
  const folder = tree.folder
  const effective: View = removed && base ? 'diff' : view === 'diff' && !canDiff ? 'code' : view

  return (
    <section className="code-file">
      <header className="code-file-head">
        <span className="mono code-file-name">{path.split('/').pop()}</span>
        {change && <span className={`change change-${change}`}>{change}</span>}
        {f && <span className="muted">{f.content !== null ? `${plural(lineCount(f.content), 'line')} · ` : ''}{bytes(f.size)}</span>}
        <span className="code-file-tools">
          <span className="segmented" role="tablist" aria-label="Show">
            <button type="button" role="tab" aria-selected={effective === 'code'} disabled={removed} onClick={() => onView('code')}>
              Code
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={effective === 'diff'}
              disabled={!canDiff}
              title={canDiff ? `What changed against ${base}` : 'Not changed here'}
              onClick={() => onView('diff')}
            >
              Diff
            </button>
            <button type="button" role="tab" aria-selected={effective === 'blame'} disabled={removed} onClick={() => onView('blame')}>
              Blame
            </button>
          </span>
          {effective === 'code' && (
            <button type="button" className="btn btn-ghost" aria-pressed={wrap} onClick={() => setWrap((w) => !w)} title="Wrap long lines">
              Wrap
            </button>
          )}
          {folder && !removed && <OpenInEditor file={joinPath(folder, path)} line={lines?.[0]} note={folderNote(tree)} />}
        </span>
      </header>
      {effective === 'diff' && base && <DiffView base={base as Ref} head={at as Ref} path={path} kind={change ?? 'modified'} />}
      {effective === 'blame' && <BlameView attempt={tree.attempt ?? 0} path={path} at={at} />}
      {effective === 'code' && (
        <>
          {file.error && <ErrorNote error={file.error} />}
          {!f && !file.error && <Skeleton rows={8} />}
          {f && f.content === null && (
            <div className="diff-note">
              {f.too_large ? 'Too large to show' : 'Binary file'} · {bytes(f.size)}
            </div>
          )}
          {f && f.content !== null && <CodeLines text={f.content} path={path} wrap={wrap} lines={lines} onLines={onLines} />}
        </>
      )}
    </section>
  )
}

function folderNote(t: TreeResponse): string | undefined {
  if (t.ref.startsWith('branch:') && t.kind === 'commit') return 'Opens the file in that workspace’s folder, which has whatever branch is checked out there.'
  if (t.kind === 'commit' && !t.ref.startsWith('branch:')) return 'Opens the file in this checkout, which may hold a different version.'
  return undefined
}

function lineCount(text: string): number {
  if (!text) return 0
  const n = text.split('\n').length
  return text.endsWith('\n') ? n - 1 : n
}

function OpenInEditor({ file, line, note }: { file: string; line?: number; note?: string }) {
  const [editor, setEditor] = useState<Editor>(preferredEditor)
  const title = `${file}${line ? `:${line}` : ''}${note ? `\n${note}` : ''}`
  if (inEditor)
    return (
      <button type="button" className="btn btn-ghost" title={title} onClick={() => openInEditor(file, line)}>
        Open in editor
      </button>
    )
  return (
    <span className="code-open">
      <button type="button" className="btn btn-ghost" title={title} onClick={() => openInEditor(file, line, editor)}>
        Open in {EDITOR_NAMES[editor]}
      </button>
      <select
        id="code-editor"
        aria-label="Editor"
        value={editor}
        onChange={(e) => {
          const v = e.target.value as Editor
          setEditor(v)
          setPreferredEditor(v)
        }}
      >
        <option value="vscode">VS Code</option>
        <option value="cursor">Cursor</option>
      </select>
    </span>
  )
}

/** Escape text for the plain (unhighlighted) view. */
const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')

function CodeLines({
  text,
  path,
  wrap,
  lines,
  onLines,
}: {
  text: string
  path: string
  wrap: boolean
  lines: [number, number] | null
  onLines: (l: [number, number] | null) => void
}) {
  const plain = useMemo(() => {
    const l = text.split('\n')
    if (text.endsWith('\n')) l.pop()
    return l.map(esc)
  }, [text])
  const [html, setHtml] = useState<{ text: string; lines: string[] } | null>(null)
  useEffect(() => {
    let live = true
    highlightLines(text, path).then(
      (h) => live && h && h.length === plain.length && setHtml({ text, lines: h }),
      () => {},
    )
    return () => {
      live = false
    }
  }, [text, path, plain.length])
  const rows = html?.text === text ? html.lines : plain
  const anchor = useRef<number | null>(null)
  const body = useRef<HTMLDivElement>(null)
  const first = lines?.[0]
  useEffect(() => {
    if (!first) return
    body.current?.querySelector(`[data-line="${first}"]`)?.scrollIntoView({ block: 'center' })
  }, [first, rows.length])

  const chunks: number[] = []
  for (let i = 0; i < rows.length; i += CHUNK) chunks.push(i)
  const pick = (n: number, shift: boolean) => {
    if (shift && anchor.current) onLines([Math.min(anchor.current, n), Math.max(anchor.current, n)])
    else {
      anchor.current = n
      onLines(lines && lines[0] === n && lines[1] === n ? null : [n, n])
    }
  }
  if (rows.length === 0) return <div className="diff-note">Empty file</div>
  return (
    <div ref={body} className={`code-lines hljs-scope${wrap ? ' is-wrap' : ''}`} style={{ ['--ln-w' as string]: `${String(rows.length).length + 1}ch` }}>
      {chunks.map((start) => (
        <Chunk key={start} rows={rows} start={start} sel={lines} onPick={pick} />
      ))}
    </div>
  )
}

const Chunk = memo(function Chunk({
  rows,
  start,
  sel,
  onPick,
}: {
  rows: string[]
  start: number
  sel: [number, number] | null
  onPick: (n: number, shift: boolean) => void
}) {
  const end = Math.min(rows.length, start + CHUNK)
  const out: ReactNode[] = []
  for (let i = start; i < end; i++) {
    const n = i + 1
    const on = !!sel && n >= sel[0] && n <= sel[1]
    out.push(
      <div key={n} className={`cv-row${on ? ' is-sel' : ''}`} id={`L${n}`}>
        <a
          className="cv-ln"
          data-line={n}
          href={`#L${n}`}
          onClick={(e) => {
            e.preventDefault()
            onPick(n, e.shiftKey)
          }}
        >
          {n}
        </a>
        <code className="cv-code" dangerouslySetInnerHTML={{ __html: rows[i] || ' ' }} />
      </div>,
    )
  }
  return (
    <div className="cv-chunk" style={{ containIntrinsicSize: `auto ${(end - start) * 20}px` }}>
      {out}
    </div>
  )
})

// ---- go to file -------------------------------------------------------------------

function Marked({ text, at }: { text: string; at: number[] }) {
  const set = new Set(at)
  return (
    <>
      {[...text].map((ch, i) =>
        set.has(i) ? (
          <b key={i} className="code-hit">
            {ch}
          </b>
        ) : (
          ch
        ),
      )}
    </>
  )
}

function GoToFile({ at, onClose, onOpen }: { at: string; onClose: () => void; onOpen: (p: string) => void }) {
  const all = useResource(`paths:${at}`, () => api.paths(at))
  const [q, setQ] = useState('')
  const [sel, setSel] = useState(0)
  const changed = useMemo(() => new Map((all.data?.changes ?? []).map((c) => [c.path, c.kind])), [all.data])
  const hits = useMemo(() => fuzzyFilter(all.data?.paths ?? [], q, 60), [all.data, q])
  const cur = Math.min(sel, hits.length - 1)
  const open = (p?: string) => {
    if (!p) return
    onClose()
    onOpen(p)
  }
  useHotkeys(
    [
      { keys: 'esc', description: 'Close', handler: onClose, inInputs: true },
      { keys: ['down', 'mod+j'], description: 'Next', inInputs: true, handler: () => setSel(Math.min(cur + 1, hits.length - 1)) },
      { keys: ['up', 'mod+k'], description: 'Previous', inInputs: true, handler: () => setSel(Math.max(cur - 1, 0)) },
      { keys: 'enter', description: 'Open', inInputs: true, handler: () => open(hits[cur]?.path) },
    ],
    { exclusive: true },
  )
  return (
    <div className="overlay overlay-top" onMouseDown={onClose}>
      <div className="dialog palette" role="dialog" aria-label="Go to file" onMouseDown={(e) => e.stopPropagation()}>
        <div className="palette-input">
          <Icon name="file" />
          <input
            id="code-goto"
            autoFocus
            value={q}
            placeholder={all.data ? `Go to file (${all.data.paths?.length ?? 0} files)…` : 'Loading files…'}
            onChange={(e) => {
              setQ(e.target.value)
              setSel(0)
            }}
          />
        </div>
        {all.error && <ErrorNote error={all.error} />}
        <ul className="palette-list" role="listbox">
          {all.data && hits.length === 0 && <li className="palette-empty">No file matches</li>}
          {hits.map((h, i) => {
            const cut = h.path.lastIndexOf('/') + 1
            return (
              <li
                key={h.path}
                role="option"
                aria-selected={i === cur}
                className="palette-item code-goto-item"
                onMouseMove={() => setSel(i)}
                onClick={() => open(h.path)}
              >
                <Icon name="file" />
                <span className="palette-label mono">
                  <Marked text={h.path.slice(cut)} at={h.at.filter((x) => x >= cut).map((x) => x - cut)} />
                </span>
                <span className="palette-hint mono">
                  <Marked text={h.path.slice(0, cut)} at={h.at.filter((x) => x < cut)} />
                </span>
                {changed.get(h.path) && <ChangeDot kind={changed.get(h.path)} />}
              </li>
            )
          })}
        </ul>
        {all.data?.truncated && <p className="muted code-note">Only the first {all.data.paths?.length} files are listed.</p>}
      </div>
    </div>
  )
}

// ---- search -----------------------------------------------------------------------

function SearchPanel({
  at,
  q,
  inputRef,
  onQuery,
  onOpen,
}: {
  at: string
  q: string
  inputRef: React.RefObject<HTMLInputElement | null>
  onQuery: (q: string) => void
  onOpen: (path: string, line: number) => void
}) {
  const [text, setText] = useState(q)
  const [regex, setRegex] = useState(false)
  const [kase, setKase] = useState(false)
  const [glob, setGlob] = useState('')
  const [submitted, setSubmitted] = useState({ q, regex, kase, glob })
  const key = submitted.q ? `search:${at}:${submitted.q}:${submitted.regex}:${submitted.kase}:${submitted.glob}` : null
  const res = useResource(key, () => api.search(at, submitted.q, { regex: submitted.regex, case: submitted.kase, path: submitted.glob }))
  const groups = useMemo(() => {
    const m = new Map<string, SearchMatch[]>()
    for (const x of res.data?.matches ?? []) {
      const g = m.get(x.path)
      if (g) g.push(x)
      else m.set(x.path, [x])
    }
    return [...m.entries()]
  }, [res.data])
  return (
    <section className="code-search">
      <form
        className="code-search-form"
        onSubmit={(e) => {
          e.preventDefault()
          setSubmitted({ q: text, regex, kase, glob })
          onQuery(text)
        }}
      >
        <input id="code-search" ref={inputRef} className="input mono" autoFocus value={text} placeholder="Search the code" onChange={(e) => setText(e.target.value)} />
        <input id="code-search-path" className="input mono code-search-glob" value={glob} placeholder="in paths: src, *.rs" onChange={(e) => setGlob(e.target.value)} />
        <label className="code-toggle" title="Regular expression">
          <input type="checkbox" checked={regex} onChange={(e) => setRegex(e.target.checked)} /> .*
        </label>
        <label className="code-toggle" title="Match case">
          <input type="checkbox" checked={kase} onChange={(e) => setKase(e.target.checked)} /> Aa
        </label>
        <button type="submit" className="btn">
          Search
        </button>
      </form>
      {res.error && <ErrorNote error={res.error} />}
      {key && !res.data && !res.error && <Skeleton rows={4} />}
      {res.data && (
        <p className="muted code-note">
          {res.data.matches.length === 0
            ? `No matches in ${plural(res.data.searched, 'file')}.`
            : `${plural(res.data.matches.length, 'line')} in ${plural(res.data.files, 'file')}${res.data.truncated ? ' (stopped at the limit; narrow the search)' : ''}.`}
        </p>
      )}
      {groups.map(([path, ms]) => (
        <div key={path} className="code-search-file">
          <button type="button" className="code-search-path mono" onClick={() => onOpen(path, ms[0]!.line)}>
            {path} <span className="muted">· {ms.length}</span>
          </button>
          {ms.map((m) => (
            <button key={m.line} type="button" className="code-search-hit" onClick={() => onOpen(path, m.line)}>
              <span className="cv-ln">{m.line}</span>
              <code>
                <Ranges text={m.text} ranges={m.ranges} />
              </code>
            </button>
          ))}
        </div>
      ))}
    </section>
  )
}

/** `text` with byte ranges (from the daemon) marked. */
function Ranges({ text, ranges }: { text: string; ranges: [number, number][] }) {
  // Byte offsets to string offsets.
  const enc = new TextEncoder()
  const bytesOf = enc.encode(text)
  const dec = new TextDecoder()
  const toIdx = (b: number) => dec.decode(bytesOf.slice(0, b)).length
  const out: ReactNode[] = []
  let last = 0
  ranges.forEach(([s, e], i) => {
    const a = toIdx(s)
    const b = toIdx(e)
    if (a > last) out.push(text.slice(last, a))
    out.push(
      <mark key={i} className="code-mark">
        {text.slice(a, b)}
      </mark>,
    )
    last = b
  })
  out.push(text.slice(last))
  return <>{out}</>
}
