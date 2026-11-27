// "What agents may do": a settings page, out of the way of the main UI.
// Per workspace: how often its agents ask, the person's Claude Code setup
// they inherit (settings files, allow/ask/deny rules, MCP servers), and the
// guard rails agentvc always adds. Then each open agent's mode.

import { useState } from 'react'
import { Link } from 'react-router'
import { api } from '../api/client'
import { useLive } from '../api/live'
import type { AgentPermissions, ClaudeSetup, WorkspacePermissions } from '../api/types'
import { useResource } from '../api/useResource'
import { ErrorNote, Skeleton } from '../components/bits'
import { useToast } from '../components/Toaster'
import { agentLink, agentState, PERMISSION_HELP, PERMISSION_WORDS, RUNNER_WORDS } from '../core/agents'
import { plural } from '../lib/format'
import { useReview } from './reviewContext'
import './agents.css'

const FROM_WORDS: Record<WorkspacePermissions['permissions_from'], string> = {
  workspace: 'set for this workspace',
  repo: 'repo default (git config agentvc.agent-permissions)',
  default: 'default',
}

function Rules({ title, rules }: { title: string; rules: string[] }) {
  if (!rules.length) return null
  return (
    <div className="pm-rules">
      <span className="pm-rules-h">{title}</span>
      {rules.map((r) => (
        <code key={r} className="pm-rule">
          {r}
        </code>
      ))}
    </div>
  )
}

function Setup({ s }: { s: ClaudeSetup }) {
  const [open, setOpen] = useState(false)
  const found = s.sources.filter((x) => x.found)
  const rules = s.allow.length + s.ask.length + s.deny.length
  return (
    <div className="pm-setup">
      <div className="pm-row">
        <span className="pm-k">Your setup</span>
        <span>
          {found.length ? found.map((x) => x.label.replace(/ \(.*\)$/, '')).join(', ') : 'no Claude Code settings found'}
          {' · '}
          {plural(rules, 'rule')}
          {' · '}
          <button type="button" className="linkish" onClick={() => setOpen((o) => !o)}>
            {open ? 'Hide details' : 'Details'}
          </button>
        </span>
      </div>
      <div className="pm-row">
        <span className="pm-k">MCP servers</span>
        <span className="pm-chips">
          {s.mcp_servers.map((m) => (
            <span key={m.name} className="ag-chip" title={`${m.scope} server`}>
              {m.name}
            </span>
          ))}
        </span>
      </div>
      {open && (
        <div className="pm-details">
          <ul className="pm-sources">
            {s.sources.map((x) => (
              <li key={x.path} className={x.found ? '' : 'muted'}>
                <span>{x.label}</span> <code>{x.path}</code>{' '}
                <span className="muted">
                  {!x.found ? '· not found' : x.via === 'agentvc' ? '· passed on by agentvc' : '· read by Claude Code'}
                </span>
              </li>
            ))}
          </ul>
          <Rules title="Allowed without asking" rules={s.allow} />
          <Rules title="Always asks" rules={s.ask} />
          <Rules title="Never allowed" rules={s.deny} />
          {s.dropped.length > 0 && (
            <p className="muted ag-hint">
              Not passed on, so agents stay in their own copy: <code>{s.dropped.join(', ')}</code>
            </p>
          )}
        </div>
      )}
    </div>
  )
}

function WorkspaceCard({ w, steer }: { w: WorkspacePermissions; steer: boolean }) {
  const toast = useToast()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const change = async (permissions: AgentPermissions | undefined) => {
    setBusy(true)
    try {
      const cur = await api.agentSettings(w.workspace)
      await api.setAgentSettings(w.workspace, { ...cur.settings, permissions })
      toast.ok(`New agents in ${w.workspace_name} use: ${PERMISSION_WORDS[permissions ?? 'auto']}.`, { undo: false })
    } catch (err) {
      toast.error(err)
    } finally {
      setBusy(false)
      refresh()
    }
  }
  const claude = w.runner === 'claude'
  return (
    <section className="ag-card pm-card">
      <div className="pm-head">
        <span className="ag-title">{w.workspace_name}</span>
        <span className="muted mono pm-path">{w.path}</span>
      </div>
      <div className="pm-row">
        <span className="pm-k">Runner</span>
        <span>{RUNNER_WORDS[w.runner] ?? w.runner}</span>
      </div>
      {claude ? (
        <>
          <div className="pm-row">
            <span className="pm-k">Asks you</span>
            <span className="pm-level">
              {steer ? (
                <select
                  aria-label={`Permissions for ${w.workspace_name}`}
                  value={w.permissions_from === 'workspace' ? w.permissions : ''}
                  disabled={busy}
                  onChange={(e) => void change((e.target.value || undefined) as AgentPermissions | undefined)}
                >
                  <option value="">{w.permissions_from === 'workspace' ? 'Default' : `${PERMISSION_WORDS[w.permissions]} (${FROM_WORDS[w.permissions_from]})`}</option>
                  {(Object.keys(PERMISSION_WORDS) as AgentPermissions[]).map((p) => (
                    <option key={p} value={p}>
                      {PERMISSION_WORDS[p]}
                    </option>
                  ))}
                </select>
              ) : (
                <strong>{PERMISSION_WORDS[w.permissions]}</strong>
              )}
              <span className="muted">{PERMISSION_HELP[w.permissions]}</span>
            </span>
          </div>
          <div className="pm-row">
            <span className="pm-k">Commands</span>
            <span>{w.sandbox ? 'In Claude Code’s sandbox: they can write only inside the agent’s copy' : 'Not sandboxed (turn on in the workspace’s agent settings)'}</span>
          </div>
          {w.setup && <Setup s={w.setup} />}
        </>
      ) : (
        <p className="muted ag-hint">
          {RUNNER_WORDS[w.runner] ?? 'This runner'} runs with its own permissions; agentvc passes no settings to it.
        </p>
      )}
    </section>
  )
}

export function PermissionsPage() {
  const steer = useReview().agents?.can_steer !== false
  const data = useResource('agent-permissions', () => api.agentPermissions())
  if (data.error) return <div className="page rv"><ErrorNote error={data.error} /></div>
  if (!data.data) return <div className="page rv"><Skeleton rows={6} /></div>
  const r = data.data
  return (
    <div className="page rv pm-page">
      <div>
        <h1 className="rv-title">What agents may do</h1>
        <p className="muted">
          Agents run Claude Code with your own setup: your settings and allow rules, your MCP servers, and this checkout&rsquo;s local settings. Each workspace
          picks how often its agents ask you.
        </p>
      </div>

      <section>
        <h2 className="rv-h2">Always</h2>
        <ul className="pm-guard">
          {r.guard_rails.map((g) => (
            <li key={g}>{g}</li>
          ))}
        </ul>
      </section>

      <section>
        <h2 className="rv-h2">Workspaces</h2>
        <div className="ag-cards">
          {r.workspaces.map((w) => (
            <WorkspaceCard key={w.workspace} w={w} steer={steer} />
          ))}
        </div>
      </section>

      <section>
        <h2 className="rv-h2">Agents now</h2>
        {r.agents.length === 0 ? (
          <p className="muted">No open agents.</p>
        ) : (
          <table className="pm-table">
            <tbody>
              {r.agents.map((a) => (
                <tr key={a.id}>
                  <td>
                    <Link to={agentLink(a.id)}>{a.title}</Link>
                  </td>
                  <td className="muted">{a.workspace_name}</td>
                  <td className="muted">{agentState({ state: a.state, stale: false }).label}</td>
                  <td>
                    {a.runner === 'claude'
                      ? `${PERMISSION_WORDS[a.permissions ?? 'edits'].split(':')[0]}${a.sandbox ? ', sandboxed' : ''}`
                      : (RUNNER_WORDS[a.runner] ?? a.runner)}
                    {a.claude_mode && <span className="muted mono"> · {a.claude_mode}</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <section>
        <h2 className="rv-h2">The levels</h2>
        <dl className="pm-levels">
          {(Object.keys(PERMISSION_HELP) as AgentPermissions[]).map((p) => (
            <div key={p}>
              <dt>{PERMISSION_WORDS[p]}</dt>
              <dd className="muted">{PERMISSION_HELP[p]}</dd>
            </div>
          ))}
        </dl>
      </section>
    </div>
  )
}
