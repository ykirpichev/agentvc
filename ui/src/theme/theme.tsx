import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from 'react'
import { cssColor, embedMode, hostBg, hostTheme, onHost } from '../core/embed'

export type ThemePref = 'system' | 'light' | 'dark'
const ORDER: ThemePref[] = ['system', 'light', 'dark']

const STORAGE_KEY = 'agentvc.theme'

interface ThemeState {
  pref: ThemePref
  resolved: 'light' | 'dark'
  setPref: (p: ThemePref) => void
  cycle: () => void
}

const ThemeContext = createContext<ThemeState | null>(null)

function readPref(): ThemePref {
  try {
    const v = localStorage.getItem(STORAGE_KEY)
    if (v === 'light' || v === 'dark' || v === 'system') return v
  } catch {
    // Storage can be unavailable (private mode); fall back to the system.
  }
  return 'system'
}

const media = () => window.matchMedia('(prefers-color-scheme: dark)')

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [pref, setPref] = useState<ThemePref>(() => hostTheme ?? readPref())
  const [systemDark, setSystemDark] = useState(() => media().matches)

  useEffect(() => {
    const m = media()
    const on = (e: MediaQueryListEvent) => setSystemDark(e.matches)
    m.addEventListener('change', on)
    return () => m.removeEventListener('change', on)
  }, [])

  // An editor host follows its own theme and tells us when it changes.
  const [bg, setBg] = useState<string | null>(hostBg)
  useEffect(
    () =>
      onHost((msg) => {
        if (msg.type !== 'agentvc.theme') return
        setPref(msg.theme)
        setBg(cssColor(msg.bg) ?? null)
      }),
    [],
  )
  // Inside an editor, paint its own background so the frame blends in.
  useEffect(() => {
    if (embedMode) document.documentElement.style.background = bg ?? ''
  }, [bg])

  const resolved = pref === 'system' ? (systemDark ? 'dark' : 'light') : pref

  useEffect(() => {
    document.documentElement.dataset.theme = resolved
    document.documentElement.style.colorScheme = resolved
    if (embedMode) document.documentElement.dataset.embed = embedMode
    try {
      if (!embedMode) localStorage.setItem(STORAGE_KEY, pref)
    } catch {
      // Not persisted; fine.
    }
  }, [pref, resolved])

  const value = useMemo<ThemeState>(
    () => ({
      pref,
      resolved,
      setPref,
      cycle: () => setPref((p) => ORDER[(ORDER.indexOf(p) + 1) % ORDER.length]!),
    }),
    [pref, resolved],
  )
  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>
}

// eslint-disable-next-line react-refresh/only-export-components
export function useTheme(): ThemeState {
  const t = useContext(ThemeContext)
  if (!t) throw new Error('useTheme needs a <ThemeProvider>')
  return t
}
