import { memo, startTransition, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { Link, useNavigate, useSearchParams } from 'react-router'
import { api, ref } from '../api/client'
import type { CompareColumn, CompareFile, CompareResponse } from '../api/types'
import { useResource } from '../api/useResource'
import { Empty, ErrorNote, Kbd, PageHeader, Skeleton, StatusPill } from '../components/bits'
import { Icon } from '../components/Icon'
import { ActionButtons, useAttemptActions } from '../components/ReviewActions'
import { StatChips } from '../components/StatChips'
import { useHotkeys } from '../keyboard/hotkeys'
import { align, lineEdits, splitLines, type Block, type Line } from '../lib/align'
import { actorName, plural, timeAgo } from '../lib/format'

/** Unchanged lines kept around each change. */
const CONTEXT = 3

export function Compare() {
  const [params] = useSearchParams()
  const ids = (params.get('a') ?? '')
    .split(',')
    .map((x) => Number(x.replace(/^a/, '')))
    .filter((n) => Number.isInteger(n) && n > 0)
  const key = ids.join(',')
  const { data, error, loading } = useResource(ids.length ? `compare:${key}` : null, () => api.compare(ids))
  return (
    <div className="page page-full">
      {ids.length < 2 && (
        <Empty title="Pick 2 to 4 attempts">
          Select attempts on a task with <Kbd keys="space" />, then press <Kbd keys="c" />.
        </Empty>
      )}
      {error && <ErrorNote error={error} />}
      {loading && <Skeleton rows={8} />}
      {data && <CompareView key={key} data={data} />}
    </div>
  )
}

function CompareView({ data }: { data: CompareResponse }) {
  const navigate = useNavigate()
  const cols = data.columns
  const n = cols.length
  const taskId = cols[0]?.attempt.task_id
  const task = useResource(taskId ? `task:${taskId}` : null, () => api.task(taskId!))
  const siblings = task.data?.attempts ?? cols.map((c) => c.attempt)

  const [focus, setFocus] = useState(0)
  const focused = cols[Math.min(focus, n - 1)]
  const actions = useAttemptActions(focused?.attempt, siblings, { group: 'Review focused column' })

  const differing = data.files.filter((f) => !f.same)
  const same = data.files.filter((f) => f.same)
  const [showSame, setShowSame] = useState(false)
  const visible = showSame ? data.files : differing
  const [fileIdx, setFileIdx] = useState(0)
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set())
  const sections = useRef(new Map<string, HTMLElement>())

  const goFile = (i: number) => {
    const f = visible[i]
    if (!f) return
    setFileIdx(i)
    sections.current.get(f.path)?.scrollIntoView({ block: 'start', behavior: 'smooth' })
  }
  const toggleCollapsed = (path: string) =>
    setCollapsed((s) => {
      const next = new Set(s)
      if (next.has(path)) next.delete(path)
      else next.add(path)
      return next
    })

  useHotkeys([
    { keys: ['j', 'down'], description: 'Next file', group: 'Files', handler: () => goFile(Math.min(fileIdx + 1, visible.length - 1)) },
    { keys: ['k', 'up'], description: 'Previous file', group: 'Files', handler: () => goFile(Math.max(fileIdx - 1, 0)) },
    {
      keys: 'o',
      description: 'Fold or unfold file',
      group: 'Files',
      handler: () => visible[fileIdx] && toggleCollapsed(visible[fileIdx].path),
    },
    {
      keys: 'i',
      description: 'Show or hide identical files',
      group: 'Files',
      handler: () => setShowSame((v) => !v),
    },
    { keys: ['h', 'left'], description: 'Focus column to the left', group: 'Columns', handler: () => setFocus((f) => Math.max(0, f - 1)) },
    { keys: ['l', 'right'], description: 'Focus column to the right', group: 'Columns', handler: () => setFocus((f) => Math.min(n - 1, f + 1)) },
    ...cols.map((c, i) => ({
      keys: String(i + 1),
      description: `Focus a${c.attempt.id}`,
      group: 'Columns',
      hidden: i > 0,
      handler: () => setFocus(i),
    })),
    {
      keys: ['u', 'backspace'],
      description: 'Back to the task',
      group: 'Compare',
      handler: () => navigate(taskId ? `/tasks/${taskId}` : '/tasks'),
    },
  ])

  const gridStyle = { '--cols': n } as React.CSSProperties
  return (
    <>
      <Link to={taskId ? `/tasks/${taskId}` : '/tasks'} className="back">
        <Icon name="back" size={14} /> {taskId ? `Task t${taskId}` : 'Tasks'}
      </Link>
      <PageHeader
        eyebrow={
          <>
            Against s{data.base.id} (a{data.base.attempt_id}) · {plural(differing.length, 'file')} differ
            {same.length > 0 && <> · {same.length} identical</>}
          </>
        }
        title={task.data?.task.goal ?? `Compare ${cols.map((c) => `a${c.attempt.id}`).join(' · ')}`}
      >
        <span className="muted hint-line">
          <Kbd keys="h" /> <Kbd keys="l" /> column · <Kbd keys="j" /> <Kbd keys="k" /> file · <Kbd keys="p" /> pick
        </span>
      </PageHeader>

      <div className="cmp-heads" style={gridStyle}>
        {cols.map((c, i) => (
          <ColumnHead
            key={c.attempt.id}
            col={c}
            index={i}
            focused={i === focus}
            onFocus={() => setFocus(i)}
            actions={i === focus ? <ActionButtons actions={actions} compact /> : null}
          />
        ))}
      </div>

      {data.files.length === 0 ? (
        <Empty title="No changes">None of these attempts changed anything since s{data.base.id}.</Empty>
      ) : (
        <div className="cmp-body">
          <aside className="panel cmp-files">
            <div className="panel-head">
              <span>Files</span>
              <span className="muted">{data.files.length}</span>
            </div>
            <ul className="changes" role="listbox">
              {visible.map((f, i) => (
                <FileNavRow key={f.path} file={f} selected={i === fileIdx} onClick={() => goFile(i)} />
              ))}
            </ul>
            {same.length > 0 && (
              <button className="same-toggle" onClick={() => setShowSame((v) => !v)}>
                <Icon name={showSame ? 'minus' : 'plus'} size={11} />
                {showSame ? 'Hide' : 'Show'} {plural(same.length, 'file')} identical in all
                <Kbd keys="i" />
              </button>
            )}
          </aside>
          <div className="cmp-files-main">
            {differing.length === 0 && !showSame && (
              <div className="agree-note">
                <Icon name="check" size={14} /> All attempts made the same changes. Press <Kbd keys="i" /> to
                review them.
              </div>
            )}
            {visible.map((f, i) => (
              <section
                key={f.path}
                ref={(el) => {
                  if (el) sections.current.set(f.path, el)
                  else sections.current.delete(f.path)
                }}
                className={`cmp-file ${i === fileIdx ? 'current' : ''}`}
                onMouseDown={() => setFileIdx(i)}
              >
                <FileHeader
                  file={f}
                  cols={cols}
                  open={!collapsed.has(f.path)}
                  onToggle={() => toggleCollapsed(f.path)}
                  style={gridStyle}
                />
                {!collapsed.has(f.path) && <FileGrid file={f} data={data} style={gridStyle} />}
              </section>
            ))}
          </div>
        </div>
      )}
      {actions.node}
    </>
  )
}

function ColumnHead({
  col,
  index,
  focused,
  onFocus,
  actions,
}: {
  col: CompareColumn
  index: number
  focused: boolean
  onFocus: () => void
  actions: React.ReactNode
}) {
  const a = col.attempt
  return (
    <div className={`cmp-head status-${a.status} ${focused ? 'focused' : ''}`} onMouseDown={onFocus}>
      <div className="cmp-head-top">
        <span className="col-key">{index + 1}</span>
        <Link to={`/attempts/${a.id}`} className="row-id ref">
          a{a.id}
        </Link>
        <span className="cmp-head-name">
          {actorName(a.actor)}
          {a.model && <span className="muted"> · {a.model}</span>}
        </span>
        <StatusPill status={a.status} />
      </div>
      <StatChips
        stats={col.stats}
        lines={{ added: col.lines_added, removed: col.lines_removed }}
        files={col.files_changed}
      />
      <p className={`agent-summary ${col.stats.summary ? '' : 'muted'}`}>
        {col.stats.summary ?? a.prompt ?? `No summary · updated ${timeAgo(col.stats.last_activity)}`}
      </p>
      {a.reason && <div className="cmp-reason">{a.reason}</div>}
      <div className="cmp-head-actions">{actions}</div>
    </div>
  )
}

const KIND_SIGN = { added: 'A', modified: 'M', removed: 'D', unchanged: '·' } as const

function FileNavRow({ file, selected, onClick }: { file: CompareFile; selected: boolean; onClick: () => void }) {
  const slash = file.path.lastIndexOf('/')
  return (
    <li role="option" aria-selected={selected} className="change-row cmp-nav-row" onClick={onClick} title={file.path}>
      <span className="change-path">
        {slash >= 0 && <span className="dir">{file.path.slice(0, slash + 1)}</span>}
        {file.path.slice(slash + 1)}
      </span>
      <span className="cell-signs">
        {file.cells.map((c, i) => (
          <span key={i} className={`cell-sign change-${c.kind}`} title={`Column ${i + 1}: ${c.kind}`}>
            {KIND_SIGN[c.kind]}
          </span>
        ))}
      </span>
    </li>
  )
}

function FileHeader({
  file,
  cols,
  open,
  onToggle,
  style,
}: {
  file: CompareFile
  cols: CompareColumn[]
  open: boolean
  onToggle: () => void
  style: React.CSSProperties
}) {
  return (
    <header className="cmp-file-head">
      <button className="cmp-file-path" onClick={onToggle} title={open ? 'Fold (o)' : 'Unfold (o)'}>
        <Icon name="chevron" size={12} className={open ? 'rot' : ''} />
        <Icon name="file" size={14} />
        <span className="diff-path">{file.path}</span>
        {file.same && <span className="agree-chip">identical in all {cols.length}</span>}
      </button>
      <div className="cmp-file-cells" style={style}>
        {file.cells.map((c, i) => (
          <span key={i} className="cmp-file-cell">
            {c.kind === 'unchanged' ? (
              <span className="muted">{c.oid ? 'unchanged' : 'absent'}</span>
            ) : (
              <>
                <span className={`change change-${c.kind}`}>{c.kind}</span>
                {c.binary ? (
                  <span className="muted">binary</span>
                ) : (
                  <span className="diff-stat">
                    <span className="add">+{c.added}</span>
                    <span className="del">−{c.removed}</span>
                  </span>
                )}
              </>
            )}
            <span className="muted cell-col">a{cols[i]!.attempt.id}</span>
          </span>
        ))}
      </div>
    </header>
  )
}

interface Loaded {
  binary: boolean
  blocks: Block[]
  base: string[]
}

/** Fetch the base and each column's version once per blob, then align. */
async function loadFile(file: CompareFile, data: CompareResponse): Promise<Loaded> {
  const baseHas = file.cells.some((c) => c.kind === 'modified' || c.kind === 'removed' || (c.kind === 'unchanged' && c.oid))
  const baseRef = ref.snapshot(data.base.id)
  const byOid = new Map<string, Promise<string | null>>()
  const text = async (snap: number, oid: string) => {
    let p = byOid.get(oid)
    if (!p) {
      p = api.file(ref.snapshot(snap), file.path).then((f) => f.content)
      byOid.set(oid, p)
    }
    return p
  }
  const baseText = baseHas ? (await api.file(baseRef, file.path)).content : ''
  const colTexts = await Promise.all(
    file.cells.map((c, i) => {
      if (c.binary) return Promise.resolve(null)
      if (!c.oid) return Promise.resolve('')
      if (c.kind === 'unchanged') return Promise.resolve(baseText)
      return text(data.columns[i]!.head.id, c.oid)
    }),
  )
  if (baseText === null || colTexts.some((t) => t === null)) return { binary: true, blocks: [], base: [] }
  // Paired with `cmp-rendered` in AlignedGrid: the gap is diff plus render,
  // the cost the 5,000-line benchmark holds under 100 ms.
  performance.mark(`cmp-loaded:${file.path}`)
  const base = splitLines(baseText)
  const blocks = align(
    base,
    colTexts.map((t) => lineEdits(base, splitLines(t!))),
  )
  return { binary: false, blocks, base }
}

function FileGrid({ file, data, style }: { file: CompareFile; data: CompareResponse; style: React.CSSProperties }) {
  const key = `cmpfile:${data.base.id}:${file.path}:${file.cells.map((c) => c.oid).join(',')}`
  const res = useResource(key, () => loadFile(file, data))
  const [opened, setOpened] = useState<Set<number>>(new Set())
  if (res.error) return <ErrorNote error={res.error} />
  if (!res.data) return <Skeleton rows={4} />
  if (res.data.binary) return <div className="diff-note">Binary or large file in at least one attempt.</div>
  return (
    <AlignedGrid
      blocks={res.data.blocks}
      base={res.data.base}
      n={file.cells.length}
      style={style}
      opened={opened}
      onOpen={(i) => setOpened((s) => new Set(s).add(i))}
      path={file.path}
    />
  )
}

/** Rows taller than this split into slices so off-screen parts can skip layout. */
const SLICE = 100
/** Lines per chunk; each chunk lays out only when near the viewport. */
const CHUNK = 150
/** Lines rendered in the first commit; enough to fill a tall screen. */
const FIRST_PAINT_LINES = 600

interface Row {
  key: string
  lines: number
  node: React.ReactNode
}

/**
 * The hot path: a 5,000-line diff must render in under 100 ms. Lines are one
 * element each (gutter numbers come from CSS), rows are small grids so each
 * block's cells share a height across columns, and rows are grouped into
 * chunks with `content-visibility: auto` so the browser lays out only what is
 * on screen.
 */
const AlignedGrid = memo(function AlignedGrid({
  blocks,
  base,
  n,
  style,
  opened,
  onOpen,
  path,
}: {
  blocks: Block[]
  base: string[]
  n: number
  style: React.CSSProperties
  opened: Set<number>
  onOpen: (i: number) => void
  path: string
}) {
  // First paint renders the top of the file; the rest follows as an
  // interruptible transition so a huge diff never blocks input.
  const [complete, setComplete] = useState(false)
  useLayoutEffect(() => {
    performance.mark(`cmp-rendered:${path}`)
  }, [path])
  useEffect(() => {
    startTransition(() => setComplete(true))
  }, [])

  const rows: Row[] = []
  const pushRow = (key: string, cols: Line[][], changed: boolean[] | null) => {
    const tall = Math.max(...cols.map((c) => c.length))
    for (let s = 0; s < Math.max(tall, 1); s += SLICE) {
      const k = `${key}:${s}`
      rows.push({
        key: k,
        lines: Math.min(SLICE, tall - s),
        node: (
          <div key={k} className={`cg-row ${changed ? 'cg-change' : 'cg-same'}`}>
            {cols.map((lines, ci) => (
              <div key={ci} className={`cg-cell ${changed && !changed[ci] ? 'untouched' : ''}`}>
                <Lines lines={s === 0 && lines.length <= SLICE ? lines : lines.slice(s, s + SLICE)} />
              </div>
            ))}
          </div>
        ),
      })
    }
  }
  const same = (b: Extract<Block, { kind: 'same' }>, from: number, to: number): Line[][] =>
    Array.from({ length: n }, (_, ci) => {
      const out: Line[] = []
      for (let k = from; k < to; k++) out.push({ kind: 'ctx', text: base[k]!, n: k + 1 + b.offsets[ci]! })
      return out
    })

  blocks.forEach((b, bi) => {
    if (b.kind === 'change') return pushRow(`${bi}`, b.cols, b.changed)
    const len = b.end - b.start
    const head = b.start === 0 ? 0 : CONTEXT
    const tail = b.end === base.length ? 0 : CONTEXT
    if (opened.has(bi) || len <= head + tail + 1) return pushRow(`${bi}`, same(b, b.start, b.end), null)
    if (head) pushRow(`${bi}h`, same(b, b.start, b.start + head), null)
    rows.push({
      key: `${bi}gap`,
      lines: 1,
      node: (
        <button key={`${bi}gap`} className="cg-gap" onClick={() => onOpen(bi)}>
          ⋯ {len - head - tail} unchanged lines
        </button>
      ),
    })
    if (tail) pushRow(`${bi}t`, same(b, b.end - tail, b.end), null)
  })

  const chunks: { key: string; lines: number; rows: React.ReactNode[] }[] = []
  for (const r of rows) {
    const last = chunks[chunks.length - 1]
    if (last && last.lines + r.lines <= CHUNK) {
      last.lines += r.lines
      last.rows.push(r.node)
    } else {
      chunks.push({ key: r.key, lines: r.lines, rows: [r.node] })
    }
  }
  let budget = FIRST_PAINT_LINES
  return (
    <div className="cg" style={style}>
      {chunks.map((c) => {
        budget -= c.lines
        return complete || budget + c.lines > 0 ? (
          <div key={c.key} className="cg-chunk" style={{ containIntrinsicSize: `auto ${c.lines * 20}px` }}>
            {c.rows}
          </div>
        ) : (
          <div key={c.key} className="cg-chunk cg-pending" style={{ height: c.lines * 20 }} />
        )
      })}
    </div>
  )
})

function Lines({ lines }: { lines: Line[] }) {
  return (
    <>
      {lines.map((l, i) => (
        <div key={i} className={`cl cl-${l.kind}`} data-n={l.n}>
          {l.text || ' '}
        </div>
      ))}
    </>
  )
}
