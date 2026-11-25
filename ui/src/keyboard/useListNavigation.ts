import { useEffect, useRef, useState } from 'react'
import { useHotkeys } from './hotkeys'

interface Options {
  onOpen?: (index: number) => void
  /** Heading and noun for the shortcut overlay. */
  group?: string
  noun?: string
  /** Controlled mode: the selection lives elsewhere (e.g. the URL). */
  selected?: number
  onSelect?: (index: number) => void
}

/**
 * j/k (and arrows) move a selection through a list; enter or o opens it.
 * Returns the selected index and a ref callback that keeps the selected row
 * scrolled into view.
 */
export function useListNavigation(count: number, opts: Options = {}) {
  const [own, setOwn] = useState(0)
  const raw = opts.selected ?? own
  const setSelected = opts.onSelect ?? setOwn
  const clamped = count === 0 ? -1 : Math.max(0, Math.min(raw, count - 1))
  const rowEl = useRef<HTMLElement | null>(null)
  const noun = opts.noun ?? 'item'

  useEffect(() => {
    rowEl.current?.scrollIntoView({ block: 'nearest' })
  }, [clamped])

  useHotkeys([
    {
      keys: ['j', 'down'],
      description: `Next ${noun}`,
      group: opts.group,
      handler: () => count > 0 && setSelected(Math.min(clamped + 1, count - 1)),
    },
    {
      keys: ['k', 'up'],
      description: `Previous ${noun}`,
      group: opts.group,
      handler: () => count > 0 && setSelected(Math.max(clamped - 1, 0)),
    },
    {
      keys: ['enter', 'o'],
      description: `Open ${noun}`,
      group: opts.group,
      hidden: !opts.onOpen,
      handler: () => clamped >= 0 && opts.onOpen?.(clamped),
    },
  ])

  const rowRef = (index: number) => (el: HTMLElement | null) => {
    if (index === clamped) rowEl.current = el
  }
  return { selected: clamped, setSelected, rowRef }
}
