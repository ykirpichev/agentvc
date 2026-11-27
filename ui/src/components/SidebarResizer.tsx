import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from 'react'
import { SIDEBAR_DEFAULT, SIDEBAR_MAX as MAX, SIDEBAR_MIN as MIN } from './useSidebarWidth'

const STEP = 16

/** The handle on the sidebar's right edge: drag, arrow keys, double-click resets. */
export function SidebarResizer({
  width,
  setWidth,
}: {
  width: number
  setWidth: (w: number, save?: boolean) => void
}) {
  const drag = useRef<{ x: number; w: number } | null>(null)
  const [dragging, setDragging] = useState(false)

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return
    e.preventDefault()
    e.currentTarget.setPointerCapture(e.pointerId)
    drag.current = { x: e.clientX, w: width }
    setDragging(true)
  }
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return
    setWidth(drag.current.w + e.clientX - drag.current.x, false)
  }
  const end = (e: PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return
    setWidth(drag.current.w + e.clientX - drag.current.x)
    drag.current = null
    setDragging(false)
  }
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.shiftKey ? STEP * 4 : STEP
    if (e.key === 'ArrowLeft') setWidth(width - step)
    else if (e.key === 'ArrowRight') setWidth(width + step)
    else if (e.key === 'Home') setWidth(MIN)
    else if (e.key === 'End') setWidth(MAX)
    else if (e.key === 'Enter') setWidth(SIDEBAR_DEFAULT)
    else return
    e.preventDefault()
  }

  // While dragging, keep the resize cursor and stop text selection everywhere.
  useEffect(() => {
    document.body.classList.toggle('sidebar-resizing', dragging)
    return () => document.body.classList.remove('sidebar-resizing')
  }, [dragging])

  return (
    <div
      className={`sidebar-resizer${dragging ? ' active' : ''}`}
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize sidebar"
      aria-valuenow={width}
      aria-valuemin={MIN}
      aria-valuemax={MAX}
      tabIndex={0}
      title="Drag to resize · double-click to reset"
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onDoubleClick={() => setWidth(SIDEBAR_DEFAULT)}
      onKeyDown={onKeyDown}
    />
  )
}
