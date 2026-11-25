import type { FileChange } from '../api/types'

const SIGN = { added: 'A', modified: 'M', removed: 'D' } as const

export function ChangeList({
  changes,
  selected,
  onSelect,
  rowRef,
}: {
  changes: FileChange[]
  selected?: number
  onSelect?: (i: number) => void
  rowRef?: (i: number) => (el: HTMLElement | null) => void
}) {
  return (
    <ul className="changes" role="listbox">
      {changes.map((c, i) => {
        const slash = c.path.lastIndexOf('/')
        return (
          <li
            key={c.path}
            ref={rowRef?.(i)}
            role="option"
            aria-selected={i === selected}
            className="change-row"
            onClick={() => onSelect?.(i)}
            title={c.path}
          >
            <span className={`change-sign change-${c.kind}`}>{SIGN[c.kind]}</span>
            <span className="change-path">
              {slash >= 0 && <span className="dir">{c.path.slice(0, slash + 1)}</span>}
              {c.path.slice(slash + 1)}
            </span>
          </li>
        )
      })}
    </ul>
  )
}
