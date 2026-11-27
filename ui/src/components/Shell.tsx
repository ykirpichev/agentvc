import { useEffect, useState, type CSSProperties } from 'react'
import { Outlet, useNavigate } from 'react-router'
import { api } from '../api/client'
import { useLive } from '../api/live'
import { useResource } from '../api/useResource'
import { useHotkeys, useShortcutHelp } from '../keyboard/hotkeys'
import { useTheme } from '../theme/theme'
import { Kbd } from './bits'
import { CommandPalette } from './CommandPalette'
import { Icon, type IconName } from './Icon'
import { ShortcutHelp } from './ShortcutHelp'
import { SidebarResizer } from './SidebarResizer'
import { useSidebarWidth } from './useSidebarWidth'
import { StackPanel } from './StackPanel'
import { ToastProvider } from './Toaster'
import { embedMode, onHost, toHost } from '../core/embed'
import { hosted } from '../core/base'
import type { ReviewContext } from '../pages/reviewContext'

// No tabs: the sidebar is the stack; these stay as shortcuts and in Ctrl+K.
const NAV: { to: string; label: string; keys: string }[] = [
  { to: '/review', label: 'Home', keys: 'g r' },
  { to: '/find', label: 'Find', keys: 'g f' },
]

export function Shell() {
  const navigate = useNavigate()
  const theme = useTheme()
  const { setHelpOpen } = useShortcutHelp()
  const [paletteOpen, setPaletteOpen] = useState(false)
  const sidebar = useSidebarWidth()
  const { connection } = useLive()
  const space = useResource('space', api.space)
  const graph = useResource('graph', api.graph)
  const tasks = useResource('tasks', api.tasks)
  const agentList = useResource('agents', () => api.agents())
  const agents = agentList.data?.agents ?? []
  const needs = agentList.data?.needs_you ?? 0

  // The count of agents waiting on you, in the browser tab too.
  useEffect(() => {
    const base = document.title.replace(/^\(\d+\) /, '')
    document.title = needs > 0 ? `(${needs}) ${base}` : base
  }, [needs])

  const ctx: ReviewContext = { graph: graph.data, tasks: tasks.data, agents: agentList.data }

  useHotkeys([
    ...NAV.map((n) => ({
      keys: n.keys,
      description: `Go to ${n.label}`,
      group: 'Navigation',
      handler: () => navigate(n.to),
    })),
    {
      keys: ['mod+k', '/'],
      description: 'Search and jump',
      group: 'General',
      handler: () => setPaletteOpen(true),
    },
    {
      keys: '?',
      description: 'Keyboard shortcuts',
      group: 'General',
      handler: () => setHelpOpen(true),
    },
    {
      keys: 'shift+t',
      description: 'Switch theme (system, light, dark)',
      group: 'General',
      handler: () => theme.cycle(),
    },
  ])

  // An editor host can send us somewhere (its review tab follows the sidebar).
  useEffect(() => {
    toHost({ type: 'agentvc.ready', mode: embedMode })
    return onHost((msg) => {
      if (msg.type === 'agentvc.navigate' && msg.path.startsWith('/')) navigate(msg.path)
    })
  }, [navigate])

  const graphBox = <StackPanel tasks={tasks.data} agents={agents} />

  if (embedMode === 'sidebar') {
    // An editor's side bar: the one branch tree, nothing else.
    return (
      <ToastProvider>
        <div className="shell-embed shell-embed-sidebar">
          <div className="tree-scroll">{graphBox}</div>
        </div>
      </ToastProvider>
    )
  }
  if (embedMode === 'panel') {
    // An editor tab: the review pane alone; the side bar has the graph.
    return (
      <ToastProvider>
        <div className="shell-embed shell-embed-panel">
          <main className="main">
            <Outlet context={ctx} />
          </main>
          <ShortcutHelp />
          {paletteOpen && <CommandPalette onClose={() => setPaletteOpen(false)} />}
        </div>
      </ToastProvider>
    )
  }

  const themeIcon: IconName =
    theme.pref === 'system' ? 'system' : theme.pref === 'dark' ? 'moon' : 'sun'

  return (
    <ToastProvider>
    <div className="shell" style={{ '--sidebar-w': `${sidebar.width}px` } as CSSProperties}>
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark">
            <Icon name="logo" size={18} />
          </span>
          <div className="brand-text">
            <div className="brand-name">agentvc</div>
            <div className="brand-space" title={space.data ? `${space.data.folder ?? space.data.workdir} on ${space.data.machine ?? 'this machine'}` : undefined}>
              {space.data ? (space.data.project?.name ?? space.data.name) : '…'}
              {hosted && (
                <>
                  {' · '}
                  <a href="/" className="linkish" title="Your machines on the hosted service">
                    all machines
                  </a>
                </>
              )}
            </div>
          </div>
        </div>

        <button className="search-trigger" onClick={() => setPaletteOpen(true)}>
          <Icon name="search" />
          <span>Jump to…</span>
          <Kbd keys="mod+k" />
        </button>


        <div className="tree-scroll">{graphBox}</div>

        <div className="sidebar-foot">
          <span className={`conn conn-${connection}`} title={`Event stream: ${connection}`}>
            <span className="dot" />
            {connection === 'live' ? 'Live' : connection === 'connecting' ? 'Connecting' : 'Offline'}
          </span>
          <span className="foot-actions">
            <button
              className="icon-btn"
              onClick={() => setHelpOpen(true)}
              title="Keyboard shortcuts (?)"
              aria-label="Keyboard shortcuts"
            >
              <Icon name="keyboard" />
            </button>
            <button
              className="icon-btn"
              onClick={theme.cycle}
              title={`Theme: ${theme.pref} (⇧T)`}
              aria-label={`Theme: ${theme.pref}`}
            >
              <Icon name={themeIcon} />
            </button>
          </span>
        </div>
        <SidebarResizer width={sidebar.width} setWidth={sidebar.setWidth} />
      </aside>

      <main className="main">
        <Outlet context={ctx} />
      </main>

      <ShortcutHelp />
      {paletteOpen && <CommandPalette onClose={() => setPaletteOpen(false)} />}
    </div>
    </ToastProvider>
  )
}
