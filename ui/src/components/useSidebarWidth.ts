import { useCallback, useEffect, useState } from 'react'

// The sidebar's width, dragged from its right edge and kept between visits.
const STORAGE_KEY = 'agentvc.sidebarWidth'
export const SIDEBAR_DEFAULT = 320
export const SIDEBAR_MIN = 240
export const SIDEBAR_MAX = 720

// Never so wide that the main pane gets less than 320px.
const clamp = (w: number) =>
  Math.round(Math.max(SIDEBAR_MIN, Math.min(w, SIDEBAR_MAX, window.innerWidth - 320)))

function readWidth(): number {
  try {
    const v = Number(localStorage.getItem(STORAGE_KEY))
    if (Number.isFinite(v) && v > 0) return clamp(v)
  } catch {
    // Storage can be unavailable (private mode); use the default.
  }
  return SIDEBAR_DEFAULT
}

function saveWidth(w: number) {
  try {
    if (w === SIDEBAR_DEFAULT) localStorage.removeItem(STORAGE_KEY)
    else localStorage.setItem(STORAGE_KEY, String(w))
  } catch {
    // Not saved; the width still applies for this visit.
  }
}

/** The sidebar width to put on the shell, and the handle that changes it. */
export function useSidebarWidth() {
  const [width, setWidthState] = useState(readWidth)
  const setWidth = useCallback((w: number, save = true) => {
    const c = clamp(w)
    setWidthState(c)
    if (save) saveWidth(c)
  }, [])

  // Keep the main pane usable when the window shrinks.
  useEffect(() => {
    const onResize = () => setWidthState((w) => clamp(w))
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  return { width, setWidth }
}
