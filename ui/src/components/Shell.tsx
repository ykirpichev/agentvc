import { useEffect, useState } from 'react'
import { NavLink, Outlet, useNavigate } from 'react-router'
import { api } from '../api/client'
import { useLive } from '../api/live'
import { useResource } from '../api/useResource'
import { useHotkeys, useShortcutHelp } from '../keyboard/hotkeys'
import { useTheme } from '../theme/theme'
import { Kbd } from './bits'
import { CommandPalette } from './CommandPalette'
import { Icon, type IconName } from './Icon'
import { ShortcutHelp } from './ShortcutHelp'
import { BranchGraph } from './BranchGraph'
import { ToastProvider } from './Toaster'
import { WorkspaceStacks } from './WorkspaceStacks'
import { embedMode, onHost, toHost } from '../core/embed'
import type { ReviewContext } from '../pages/reviewContext'

const NAV: { to: string; label: string; icon: IconName; keys: string }[] = [
  { to: '/review', label: 'Review', icon: 'inbox', keys: 'g r' },
  { to: '/tasks', label: 'Tasks', icon: 'tasks', keys: 'g t' },
  { to: '/live', label: 'Live', icon: 'live', keys: 'g l' },
  { to: '/find', label: 'Find', icon: 'search', keys: 'g f' },
]

export function Shell() {
  const navigate = useNavigate()
  const theme = useTheme()
  const { setHelpOpen } = useShortcutHelp()
  const [paletteOpen, setPaletteOpen] = useState(false)
  const { connection } = useLive()
  const space = useResource('space', api.space)
  const graph = useResource('graph', api.graph)
  const tasks = useResource('tasks', api.tasks)

  const liveCount = graph.data?.attempts.filter((g) => g.attempt.status === 'exploring' && g.attempt.id !== 1).length ?? 0
  const counts: Record<string, number> = { '/live': liveCount }
  const ctx: ReviewContext = { graph: graph.data, tasks: tasks.data }

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

  const graphBox =
    graph.data && tasks.data ? (
      <BranchGraph graph={graph.data} tasks={tasks.data} />
    ) : (
      <div className="bgraph bgraph-empty muted">{graph.error ? `Graph: ${graph.error.message}` : 'Loading branches…'}</div>
    )

  if (embedMode === 'sidebar') {
    // An editor's side bar: the graph and the stacks, nothing else.
    return (
      <ToastProvider>
        <div className="shell-embed shell-embed-sidebar">
          {graphBox}
          <WorkspaceStacks />
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
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark">
            <Icon name="logo" size={18} />
          </span>
          <div className="brand-text">
            <div className="brand-name">agentvc</div>
            <div className="brand-space" title={space.data?.workdir}>
              {space.data?.name ?? '…'}
            </div>
          </div>
        </div>

        <button className="search-trigger" onClick={() => setPaletteOpen(true)}>
          <Icon name="search" />
          <span>Jump to…</span>
          <Kbd keys="mod+k" />
        </button>

        <nav className="nav nav-row">
          {NAV.map((n) => (
            <NavLink key={n.to} to={n.to} className="nav-item">
              <Icon name={n.icon} />
              <span className="nav-label">{n.label}</span>
              {(counts[n.to] ?? 0) > 0 && <span className="nav-count">{counts[n.to]}</span>}
            </NavLink>
          ))}
        </nav>

        {graphBox}
        <WorkspaceStacks />

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
