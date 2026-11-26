// Two panes stacked vertically with a handle between them to drag. The split
// is remembered per key.

import { useRef, useState, type ReactNode } from 'react'

function readRatio(key: string, fallback: number): number {
  try {
    const v = Number(localStorage.getItem(key))
    return v > 0.1 && v < 0.9 ? v : fallback
  } catch {
    return fallback
  }
}

export function SplitPane({ id, top, bottom, initial = 0.6 }: { id: string; top: ReactNode; bottom: ReactNode; initial?: number }) {
  const key = `agentvc.split.${id}`
  const [ratio, setRatio] = useState(() => readRatio(key, initial))
  const box = useRef<HTMLDivElement>(null)

  const set = (r: number) => {
    const v = Math.min(0.88, Math.max(0.12, r))
    setRatio(v)
    try {
      localStorage.setItem(key, String(v))
    } catch {
      // Not remembered; fine.
    }
  }

  const onPointerDown = (e: React.PointerEvent) => {
    e.preventDefault()
    const el = e.currentTarget as HTMLElement
    el.setPointerCapture(e.pointerId)
    const move = (ev: PointerEvent) => {
      const r = box.current?.getBoundingClientRect()
      if (r && r.height > 0) set((ev.clientY - r.top) / r.height)
    }
    const up = () => {
      el.removeEventListener('pointermove', move)
      el.removeEventListener('pointerup', up)
    }
    el.addEventListener('pointermove', move)
    el.addEventListener('pointerup', up)
  }

  return (
    <div className="vsplit" ref={box}>
      <div className="vsplit-top" style={{ flexBasis: `${ratio * 100}%` }}>
        {top}
      </div>
      <div
        className="vsplit-handle"
        role="separator"
        aria-orientation="horizontal"
        aria-valuenow={Math.round(ratio * 100)}
        aria-label="Drag to resize"
        tabIndex={0}
        title="Drag to resize"
        onPointerDown={onPointerDown}
        onDoubleClick={() => set(initial)}
        onKeyDown={(e) => {
          if (e.key === 'ArrowUp') set(ratio - 0.05)
          if (e.key === 'ArrowDown') set(ratio + 0.05)
        }}
      />
      <div className="vsplit-bottom">{bottom}</div>
    </div>
  )
}
