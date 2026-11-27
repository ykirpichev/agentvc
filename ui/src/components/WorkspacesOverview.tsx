// Home page overview: every workspace (git worktree, one per editor window)
// side by side, each with its own stack of unmerged branches.

import { Link } from 'react-router'
import { api } from '../api/client'
import type { WorkspaceInfo } from '../api/types'
import { useResource } from '../api/useResource'
import { branchLink } from '../core/links'
import { mainAhead, stackRows, trunkName, windowState } from '../core/stack'
import { prWords } from '../core/words'
import './stack.css'

export function WorkspacesOverview() {
  const ws = useResource('workspaces', api.workspaces)
  const list = ws.data && !ws.error ? ws.data.workspaces : []
  if (!list.length) return null
  return (
    <section className="wo" aria-label="Workspaces">
      <h1 className="rv-title">Workspaces</h1>
      <p className="muted wo-lead">Each workspace is a git worktree with its own stack. Pick a branch to review, rebase or move it.</p>
      <div className="wo-grid">
        {list.map((w) => (
          <Card key={w.id} w={w} />
        ))}
      </div>
    </section>
  )
}

function Card({ w }: { w: WorkspaceInfo }) {
  const live = w.branches.filter((b) => b.pr?.state !== 'merged')
  const rows = stackRows({ branches: live })
  const st = windowState(w)
  const ahead = mainAhead(w)
  return (
    <div className="wo-card">
      <div className="wo-head">
        <span className="wo-name">{w.name}</span>
        {st && <span className={`sp-chip sp-chip-${st.tone}`}>{st.label}</span>}
      </div>
      <div className="wo-path muted" title={w.path}>
        {w.path.split('/').filter(Boolean).slice(-2).join('/')}
      </div>
      {rows.length === 0 ? (
        <p className="muted wo-none">No unmerged branches.</p>
      ) : (
        <ol className="wo-stack">
          {rows.map(({ branch: b, lane }) => {
            const pr = b.pr ? prWords(b.pr.state, b.pr.status) : null
            return (
              <li key={b.name} style={{ paddingLeft: `${lane * 14}px` }}>
                <Link to={branchLink(w.id, b.name)} className={`wo-branch ${b.current ? 'cur' : ''}`}>
                  <i className={`sp-dot ${b.current ? 'sp-dot-cur' : ''} ${b.conflicts > 0 ? 'sp-dot-bad' : ''}`} />
                  <span className="sp-name">{b.name}</span>
                  <span className="muted wo-meta">
                    {b.commits} commit{b.commits === 1 ? '' : 's'}
                    {b.needs_restack ? ' · needs rebase' : ''}
                  </span>
                  {b.conflicts > 0 && <span className="sp-chip sp-chip-bad">clash</span>}
                  {b.pr && (
                    <span className="sp-chip sp-chip-pr">
                      #{b.pr.number}
                      {pr ? ` ${pr.short}` : ''}
                    </span>
                  )}
                </Link>
              </li>
            )
          })}
          <li className="wo-trunk">
            <i className="sp-dot sp-dot-trunk" />
            <span className="sp-name">{trunkName(w)}</span>
            {ahead > 0 && (
              <span className="sp-warn wo-meta">
                {ahead} new commit{ahead === 1 ? '' : 's'}
              </span>
            )}
          </li>
        </ol>
      )}
    </div>
  )
}
