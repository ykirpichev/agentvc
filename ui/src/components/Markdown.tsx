// A small, safe markdown renderer for agent replies: paragraphs, bullet and
// numbered lists, fenced code, `code`, **bold** and [links](https://...).
// It builds React elements only, so raw HTML in the text shows as text.

import type { ReactNode } from 'react'

/** Inline spans: `code`, **bold**, [text](http(s) url). */
function inline(text: string, key: string): ReactNode[] {
  const out: ReactNode[] = []
  const re = /`([^`\n]+)`|\*\*([^*\n]+?)\*\*|\[([^\]\n]+)\]\(([^)\s]+)\)/g
  let last = 0
  let i = 0
  for (let m = re.exec(text); m; m = re.exec(text)) {
    if (m.index > last) out.push(text.slice(last, m.index))
    const k = `${key}-${i++}`
    if (m[1] !== undefined) out.push(<code key={k}>{m[1]}</code>)
    else if (m[2] !== undefined) out.push(<strong key={k}>{inline(m[2], k)}</strong>)
    else if (m[4] !== undefined && /^https?:\/\//i.test(m[4])) {
      out.push(
        <a key={k} href={m[4]} target="_blank" rel="noreferrer noopener">
          {m[3]}
        </a>,
      )
    } else out.push(m[0])
    last = m.index + m[0].length
  }
  if (last < text.length) out.push(text.slice(last))
  return out
}

/** Lines joined with <br>, each with its inline spans. */
function lines(ls: string[], key: string): ReactNode[] {
  return ls.flatMap((l, i) => (i ? [<br key={`${key}-br${i}`} />, ...inline(l, `${key}-${i}`)] : inline(l, `${key}-${i}`)))
}

const BULLET = /^\s*[-*]\s+/
const NUMBER = /^\s*\d+[.)]\s+/

export function Markdown({ text, className }: { text: string; className?: string }) {
  const blocks: ReactNode[] = []
  const src = text.replace(/\r\n?/g, '\n').split('\n')
  const at = (j: number) => src[j] ?? ''
  const fence = (j: number) => at(j).trimStart().startsWith('```')
  const blank = (j: number) => at(j).trim() === ''
  const item = (j: number) => BULLET.test(at(j)) || NUMBER.test(at(j))
  let i = 0
  while (i < src.length) {
    const key = `b${blocks.length}`
    if (blank(i)) {
      i++
    } else if (fence(i)) {
      const body: string[] = []
      i++
      while (i < src.length && !fence(i)) body.push(at(i++))
      i++ // the closing fence
      blocks.push(
        <pre key={key} className="md-code">
          <code>{body.join('\n')}</code>
        </pre>,
      )
    } else if (item(i)) {
      const marker = BULLET.test(at(i)) ? BULLET : NUMBER
      const items: string[][] = []
      while (i < src.length && !blank(i) && !fence(i)) {
        const l = at(i++)
        const last = items[items.length - 1]
        if (marker.test(l) || !last) items.push([l.replace(marker, '')])
        else last.push(l.trim())
      }
      const lis = items.map((it, j) => <li key={j}>{lines(it, `${key}-${j}`)}</li>)
      blocks.push(marker === BULLET ? <ul key={key}>{lis}</ul> : <ol key={key}>{lis}</ol>)
    } else {
      const para: string[] = []
      while (i < src.length && !blank(i) && !fence(i) && !(para.length && item(i))) para.push(at(i++))
      blocks.push(<p key={key}>{lines(para, key)}</p>)
    }
  }
  return <div className={`md ${className ?? ''}`}>{blocks}</div>
}
