import { useState } from 'react'
import { useHotkeys, useShortcutHelp, type Hotkey } from '../keyboard/hotkeys'
import { Kbd } from './bits'

export function ShortcutHelp() {
  const { helpOpen } = useShortcutHelp()
  return helpOpen ? <HelpDialog /> : null
}

function HelpDialog() {
  const { setHelpOpen, active } = useShortcutHelp()
  const close = () => setHelpOpen(false)
  // Snapshot what the page offered before this dialog took over the keys.
  const [bindings] = useState(active)
  useHotkeys(
    [{ keys: ['esc', '?'], description: 'Close', handler: close, inInputs: true, hidden: true }],
    { exclusive: true },
  )

  const groups = new Map<string, Hotkey[]>()
  const seen = new Set<string>()
  for (const b of bindings) {
    const k = Array.isArray(b.keys) ? b.keys.join() : b.keys
    if (b.hidden || seen.has(k + b.description)) continue
    seen.add(k + b.description)
    const g = b.group ?? 'This page'
    groups.set(g, [...(groups.get(g) ?? []), b])
  }

  return (
    <div className="overlay" onMouseDown={close}>
      <div
        className="dialog help"
        role="dialog"
        aria-label="Keyboard shortcuts"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="dialog-head">
          <h2>Keyboard shortcuts</h2>
          <Kbd keys="esc" />
        </div>
        <div className="help-grid">
          {[...groups].map(([group, list]) => (
            <section key={group}>
              <h3>{group}</h3>
              {list.map((b) => {
                const keys = Array.isArray(b.keys) ? b.keys : [b.keys]
                return (
                  <div className="help-row" key={keys.join() + b.description}>
                    <span>{b.description}</span>
                    <span className="help-keys">
                      {keys.slice(0, 2).map((k) => (
                        <Kbd key={k} keys={k} />
                      ))}
                    </span>
                  </div>
                )
              })}
            </section>
          ))}
        </div>
      </div>
    </div>
  )
}
