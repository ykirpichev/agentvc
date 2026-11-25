/* eslint-disable react-refresh/only-export-components -- hooks live next to their provider */
// Keyboard shortcut framework.
//
// Components declare bindings with `useHotkeys`. Bindings registered later
// (deeper or more recently mounted components) win over earlier ones, and an
// `exclusive` set (a dialog) hides everything registered before it. Keys are
// written as `j`, `enter`, `?`, `shift+t`, `mod+k` (Cmd on macOS, Ctrl
// elsewhere) or sequences such as `g i`.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react'

export interface Hotkey {
  /** One or more alternatives, e.g. `['j', 'down']`. */
  keys: string | string[]
  description: string
  /** Heading in the shortcut overlay. */
  group?: string
  handler: (e: KeyboardEvent) => void
  /** Also fire while typing in an input. */
  inInputs?: boolean
  /** Hide from the shortcut overlay. */
  hidden?: boolean
}

interface Scope {
  id: string
  exclusive: boolean
  bindings: { current: Hotkey[] }
}

interface Registry {
  register: (scope: Scope) => () => void
  /** Bindings currently reachable, most specific first. */
  active: () => Hotkey[]
  helpOpen: boolean
  setHelpOpen: (open: boolean) => void
}

const HotkeyContext = createContext<Registry | null>(null)

const SEQUENCE_TIMEOUT_MS = 1000

const KEY_ALIASES: Record<string, string> = {
  arrowdown: 'down',
  arrowup: 'up',
  arrowleft: 'left',
  arrowright: 'right',
  escape: 'esc',
  ' ': 'space',
}

/** Normalize a keyboard event to the binding syntax, e.g. `shift+t`. */
export function eventCombo(e: KeyboardEvent): string {
  let key = e.key.toLowerCase()
  key = KEY_ALIASES[key] ?? key
  const parts: string[] = []
  if (e.metaKey || e.ctrlKey) parts.push('mod')
  if (e.altKey) parts.push('alt')
  // Shift is implied by symbols such as `?`, so only name it for letters
  // and named keys.
  if (e.shiftKey && (/^[a-z]$/.test(key) || key.length > 1)) parts.push('shift')
  parts.push(key)
  return parts.join('+')
}

function isTyping(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false
  const tag = target.tagName
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target.isContentEditable
}

export function HotkeyProvider({ children }: { children: ReactNode }) {
  const scopes = useRef<Scope[]>([])
  const pending = useRef<{ prefix: string; at: number } | null>(null)
  const [helpOpen, setHelpOpen] = useState(false)

  const register = useCallback((scope: Scope) => {
    scopes.current = [...scopes.current, scope]
    return () => {
      scopes.current = scopes.current.filter((s) => s.id !== scope.id)
    }
  }, [])

  const active = useCallback(() => {
    const out: Hotkey[] = []
    for (let i = scopes.current.length - 1; i >= 0; i--) {
      const s = scopes.current[i]!
      out.push(...s.bindings.current)
      if (s.exclusive) break
    }
    return out
  }, [])

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.isComposing) return
      if (['shift', 'control', 'alt', 'meta'].includes(e.key.toLowerCase())) return
      const combo = eventCombo(e)
      const typing = isTyping(e.target)
      const now = Date.now()
      const prefix =
        pending.current && now - pending.current.at < SEQUENCE_TIMEOUT_MS
          ? pending.current.prefix
          : null
      pending.current = null

      const bindings = active().filter((b) => !typing || b.inInputs)
      const keysOf = (b: Hotkey) => (Array.isArray(b.keys) ? b.keys : [b.keys])
      if (prefix) {
        const seq = `${prefix} ${combo}`
        const hit = bindings.find((b) => keysOf(b).includes(seq))
        if (hit) {
          e.preventDefault()
          hit.handler(e)
          return
        }
      }
      const hit = bindings.find((b) => keysOf(b).includes(combo))
      if (hit) {
        e.preventDefault()
        hit.handler(e)
        return
      }
      if (bindings.some((b) => keysOf(b).some((k) => k.startsWith(`${combo} `)))) {
        pending.current = { prefix: combo, at: now }
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [active])

  const value = useMemo(
    () => ({ register, active, helpOpen, setHelpOpen }),
    [register, active, helpOpen],
  )
  return <HotkeyContext.Provider value={value}>{children}</HotkeyContext.Provider>
}

function useRegistry(): Registry {
  const r = useContext(HotkeyContext)
  if (!r) throw new Error('useHotkeys needs a <HotkeyProvider>')
  return r
}

/** Register bindings while the calling component is mounted. */
export function useHotkeys(bindings: Hotkey[], opts: { exclusive?: boolean } = {}) {
  const registry = useRegistry()
  const id = useId()
  const ref = useRef(bindings)
  useEffect(() => {
    ref.current = bindings
  })
  const exclusive = opts.exclusive ?? false
  useEffect(
    () => registry.register({ id, exclusive, bindings: ref }),
    [registry, id, exclusive],
  )
}

export function useShortcutHelp() {
  const { helpOpen, setHelpOpen, active } = useRegistry()
  return { helpOpen, setHelpOpen, active }
}

/** Human-readable key label, e.g. `mod+k` → `⌘K`. */
export function keyLabel(keys: string): string[] {
  const mac = typeof navigator !== 'undefined' && /mac/i.test(navigator.platform)
  return keys.split(' ').map((step) =>
    step
      .split('+')
      .map((k) => {
        switch (k) {
          case 'mod':
            return mac ? '⌘' : 'Ctrl+'
          case 'shift':
            return '⇧'
          case 'alt':
            return mac ? '⌥' : 'Alt+'
          case 'enter':
            return '↵'
          case 'up':
            return '↑'
          case 'down':
            return '↓'
          case 'esc':
            return 'Esc'
          default:
            return k.length === 1 ? k.toUpperCase() : k
        }
      })
      .join(''),
  )
}
