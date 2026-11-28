// The agent control center: "Needs you" (home), the Start agent box on a
// branch, and one page per agent. Every card answers in place, so a person
// can keep 10 to 20 agents moving without opening each one.

import { useState, type FormEvent, type ReactNode } from 'react'
import { Link, useParams } from 'react-router'
import { api, ref } from '../api/client'
import { useLive } from '../api/live'
import type { Agent, AgentList, AgentPermissions, AgentRunner, AgentSettings, ToolModel } from '../api/types'
import { useResource } from '../api/useResource'
import { ErrorNote, Skeleton } from '../components/bits'
import { DiffView } from '../components/DiffView'
import { ReviewBar, ThreadList } from '../components/Comments'
import { scrollToThread, useThreads } from '../core/comments'
import { useToast } from '../components/Toaster'
import { agentLink, agentState, agentsOn, CLAUDE_MODELS, DEFAULT_PERMISSIONS, needsYou, PERMISSION_WORDS, PERMISSIONS_LINK, resultWords, RUNNER_WORDS, settingsLine, TOOL_MODEL_HOME, working } from '../core/agents'
import { branchLink, codeLink } from '../core/links'
import { joinPath } from '../core/editor'
import { duration, money, plural, timeAgo } from '../lib/format'
import { useReview } from './reviewContext'
import './agents.css'

/** Whether this UI may start, answer and land agents: not the hosted UI of a machine that does not allow it. */
const useCanSteer = () => useReview().agents?.can_steer !== false

function WatchOnly({ machine }: { machine?: string }) {
  return (
    <p className="muted ag-hint">
      You can watch these agents from here. To start, answer or land them, use the UI on {machine ?? 'that machine'}, or start its daemon with{' '}
      <code>--allow-remote-agents</code>.
    </p>
  )
}

/** Run an agent action, toast the outcome and refetch. */
function useAgentAction() {
  const toast = useToast()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const act = async (fn: () => Promise<unknown>, done: string) => {
    setBusy(true)
    try {
      await fn()
      toast.ok(done, { undo: false })
      return true
    } catch (e) {
      toast.error(e)
      return false
    } finally {
      setBusy(false)
      refresh()
    }
  }
  return { busy, act }
}

function Dot({ a }: { a: Agent }) {
  return <span className={`ag-dot ag-${agentState(a).tone}`} aria-hidden />
}

function Where({ a }: { a: Agent }) {
  const st = agentState(a)
  return (
    <div className="ag-where">
      <span className={`ag-chip ag-chip-${st.tone}`}>{st.label}</span>
      <span>
        {a.workspace_name} ›{' '}
        <Link className="mono" to={branchLink(a.workspace, a.start.branch)}>
          {a.start.branch}
        </Link>{' '}
        → {a.mode === 'revise' ? 'new version' : 'new branch'}
      </span>
      {a.waiting_since !== null && <span className="muted">· waiting {timeAgo(a.waiting_since)}</span>}
    </div>
  )
}

function Meta({ a }: { a: Agent }) {
  const parts = [
    a.seconds > 0 ? duration(a.seconds) : null,
    a.cost_usd > 0 ? money(a.cost_usd) : null,
    a.result && a.result.files > 0 ? `${plural(a.result.files, 'file')}, +${a.result.insertions} −${a.result.deletions}` : null,
    a.tests ? (a.tests.passed ? 'tests pass' : 'tests fail') : null,
  ].filter(Boolean)
  return parts.length ? <span className="ag-meta">{parts.join(' · ')}</span> : null
}

/** A text box with a Send button that answers or replies to an agent. */
function ReplyBox({ a, placeholder, onSent }: { a: Agent; placeholder: string; onSent?: () => void }) {
  const [text, setText] = useState('')
  const { busy, act } = useAgentAction()
  const send = async (e: FormEvent) => {
    e.preventDefault()
    if (!text.trim()) return
    if (await act(() => api.agentReply(a.id, text.trim()), `Sent to ${a.title}.`)) {
      setText('')
      onSent?.()
    }
  }
  return (
    <form className="ag-reply" onSubmit={(e) => void send(e)}>
      <input id={`reply-${a.id}`} value={text} onChange={(e) => setText(e.target.value)} placeholder={placeholder} aria-label={placeholder} />
      <button type="submit" className="btn btn-primary" disabled={busy || !text.trim()}>
        Send
      </button>
    </form>
  )
}

/** What a person can do about an agent right now, as buttons. */
function Actions({ a, compact = false }: { a: Agent; compact?: boolean }) {
  const { busy, act } = useAgentAction()
  const q = a.question
  if (q?.kind === 'approve')
    return (
      <div className="ag-acts">
        <button type="button" className="btn btn-primary" disabled={busy} onClick={() => void act(() => api.agentApprove(a.id), 'Approved.')}>
          Approve
        </button>
        <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => void act(() => api.agentDeny(a.id), 'Denied; the agent was told.')}>
          Deny
        </button>
      </div>
    )
  const done = a.state === 'ready' || a.state === 'tests_failing' || a.state === 'stopped' || a.state === 'failed'
  const changed = !!a.result?.commit
  return (
    <div className="ag-acts">
      {compact && done && (
        <Link className="btn btn-primary" to={agentLink(a.id)}>
          Review
        </Link>
      )}
      {done && changed && (
        <button
          type="button"
          className={`btn ${compact ? 'btn-ghost' : ''}`}
          disabled={busy}
          title={a.mode === 'revise' ? `Make this the new version of ${a.start.branch}; the branches above it are updated` : `Add ${a.branch} to the stack on top of ${a.start.branch}`}
          onClick={() => void act(() => api.agentLand(a.id), a.mode === 'revise' ? `Landed as the new version of ${a.start.branch}.` : `Landed ${a.branch} on ${a.start.branch}.`)}
        >
          Land on stack
        </button>
      )}
      {a.state === 'tests_failing' && (
        <button
          type="button"
          className="btn btn-ghost"
          disabled={busy}
          onClick={() =>
            void act(
              () => api.agentReply(a.id, `The checks failed (${a.tests?.command ?? 'check'}). Fix them. Last lines:\n${(a.tests?.tail ?? []).slice(-12).join('\n')}`),
              'Sent the failures back to the agent.',
            )
          }
        >
          Send failures back
        </button>
      )}
      {(a.state === 'running' || a.state === 'queued' || a.state === 'asking') && (
        <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => void act(() => api.agentStop(a.id), 'Stopping; what it did so far is kept.')}>
          Stop
        </button>
      )}
      {a.state !== 'landed' && a.state !== 'discarded' && (
        <button type="button" className="btn btn-ghost" disabled={busy} title="Throw away its worktree and branch" onClick={() => void act(() => api.agentDiscard(a.id), `Discarded ${a.title}.`)}>
          Discard
        </button>
      )}
    </div>
  )
}

/** One card in "Needs you". */
function NeedCard({ a }: { a: Agent }) {
  const steer = useCanSteer()
  const st = agentState(a)
  const q = a.question
  return (
    <article className={`ag-card ag-card-${st.tone}`}>
      <Where a={a} />
      <Link className="ag-title" to={agentLink(a.id)}>
        {a.title}
      </Link>
      {q?.kind === 'ask' && <p className="ag-q">{q.text}</p>}
      {q?.kind === 'approve' && (
        <>
          <p className="muted">It wants to run:</p>
          <pre className="ag-cmd">{q.text}</pre>
        </>
      )}
      {a.state === 'failed' && a.error && <p className="ag-err">{a.error}</p>}
      {a.state === 'tests_failing' && a.tests && (
        <p className="ag-err">
          <span className="mono">{a.tests.command}</span> failed{a.tests.tail.length ? `: ${a.tests.tail[a.tests.tail.length - 1]}` : ''}
        </p>
      )}
      {(a.state === 'ready' || a.state === 'stopped') && a.last_step && <p className="muted ag-last">{a.last_step}</p>}
      {!steer ? null : q?.kind === 'ask' ? <ReplyBox a={a} placeholder="Answer the agent…" /> : <Actions a={a} compact />}
      <Meta a={a} />
    </article>
  )
}

/** Home: agents waiting on you, then the ones working. */
export function NeedsYou({ list }: { list: AgentList | undefined }) {
  if (!list) return null
  const needs = needsYou(list.agents)
  const busy = working(list.agents)
  if (list.agents.length === 0)
    return (
      <section className="ag-home">
        <h1 className="rv-title">Needs you</h1>
        <p className="muted">
          No agents yet. Open a branch (or <b>main</b>) in the sidebar and use <b>Start agent</b>: the agent works on top of it in its own copy, and shows up here when it needs you.
        </p>
      </section>
    )
  return (
    <section className="ag-home">
      <h1 className="rv-title">
        Needs you <span className="ag-count">{needs.length}</span>
      </h1>
      {!list.can_steer && <WatchOnly machine={list.machine} />}
      {needs.length === 0 ? <p className="muted">Nothing waits on you. The agents below keep working.</p> : <div className="ag-cards">{needs.map((a) => <NeedCard key={a.id} a={a} />)}</div>}
      {busy.length > 0 && (
        <>
          <h2 className="rv-h2">
            Working <span className="muted">· {list.working} of {list.limit} slots on {list.machine}</span>
          </h2>
          <div className="ag-lines">
            {busy.map((a) => (
              <Link key={a.id} to={agentLink(a.id)} className="ag-line">
                <Dot a={a} />
                <span className="ag-line-title">
                  <span className="muted">{a.workspace_name} › </span>
                  {a.title}
                </span>
                <span className="ag-meta">{a.state === 'queued' ? 'waiting for a slot' : `${a.started_at ? timeAgo(a.started_at) : ''}${a.cost_usd > 0 ? ` · ${money(a.cost_usd)}` : ''}`}</span>
                <span className="ag-line-step muted">{a.last_step ?? 'Starting…'}</span>
              </Link>
            ))}
          </div>
        </>
      )}
    </section>
  )
}

/** The prompt box on a branch page: start an agent on top of it. */
export function StartAgent({
  workspace,
  branch,
  label,
  canRevise,
  settings,
  settingsLink,
}: {
  workspace: string
  branch: string
  label: string
  canRevise: boolean
  settings?: AgentSettings
  settingsLink?: string
}) {
  const steer = useCanSteer()
  const [prompt, setPrompt] = useState('')
  const [mode, setMode] = useState<'new' | 'revise'>('new')
  const [opts, setOpts] = useState(false)
  const [runner, setRunner] = useState<AgentRunner | ''>('')
  const [model, setModel] = useState('')
  const [check, setCheck] = useState('')
  const toast = useToast()
  const { refresh } = useLive()
  const [busy, setBusy] = useState(false)
  const start = async (e: FormEvent) => {
    e.preventDefault()
    if (!prompt.trim()) return
    setBusy(true)
    try {
      const r = await api.startAgent({
        workspace,
        from: branch,
        prompt: prompt.trim(),
        mode,
        runner: runner || undefined,
        model: model.trim() || undefined,
        check: check.trim() || undefined,
      })
      toast.ok(<>Started <Link to={agentLink(r.agent.id)}>{r.agent.title}</Link>.</>, { undo: false })
      setPrompt('')
    } catch (err) {
      toast.error(err)
    } finally {
      setBusy(false)
      refresh()
    }
  }
  if (!steer)
    return (
      <section className="ag-start">
        <h2 className="rv-h2">Start agent from {label}</h2>
        <WatchOnly />
      </section>
    )
  return (
    <form className="ag-start" onSubmit={(e) => void start(e)}>
      <h2 className="rv-h2">Start agent from {label}</h2>
      <textarea
        id={`prompt-${workspace}-${branch}`}
        value={prompt}
        onChange={(e) => setPrompt(e.target.value)}
        placeholder="What should the agent do? For example: reject dates before 1900 with a clear error, and add tests"
        rows={3}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) void start(e)
        }}
      />
      <div className="ag-start-row">
        {canRevise && (
          <div className="ag-seg" role="radiogroup" aria-label="Where the result goes">
            <button type="button" role="radio" aria-checked={mode === 'new'} className={mode === 'new' ? 'on' : ''} onClick={() => setMode('new')}>
              New branch on top
            </button>
            <button type="button" role="radio" aria-checked={mode === 'revise'} className={mode === 'revise' ? 'on' : ''} onClick={() => setMode('revise')}>
              Revise this branch
            </button>
          </div>
        )}
        <button type="button" className="linkish" onClick={() => setOpts((o) => !o)}>
          Just for this agent
        </button>
        <span className="ag-spacer" />
        <button type="submit" className="btn btn-primary" disabled={busy || !prompt.trim()}>
          Start agent
        </button>
      </div>
      {opts && (
        <div className="ag-opts">
          <label>
            Runner
            <span className="native-select">
              <select value={runner} onChange={(e) => setRunner(e.target.value as AgentRunner | '')}>
                <option value="">Workspace's ({RUNNER_WORDS[settings?.runner ?? 'claude']})</option>
                {(['claude', 'codex', 'cursor'] as const).map((r) => (
                  <option key={r} value={r}>
                    {RUNNER_WORDS[r]}
                  </option>
                ))}
              </select>
            </span>
          </label>
          <label>
            Model{' '}
            <StartModel workspace={workspace} runner={runner || settings?.runner || 'claude'} inherited={settings?.model} value={model} onChange={setModel} />
          </label>
          <label>
            Check command <input id="agent-check" value={check} onChange={(e) => setCheck(e.target.value)} placeholder={settings?.check ?? 'none'} />
          </label>
        </div>
      )}
      <p className="muted ag-hint">
        {mode === 'new'
          ? `It works in its own copy; your checkout is not touched. After you review it, it lands as a new branch on top of ${label}.`
          : `It works in its own copy. After you review it, it becomes the new version of ${label} and the branches above are updated.`}
      </p>
      <p className="muted ag-hint">
        Runs with {settingsLine(settings)}
        {settingsLink && (
          <>
            {' · '}
            <Link to={settingsLink}>workspace agent settings</Link>
          </>
        )}
      </p>
    </form>
  )
}

/** "Default (…)" for the model: the repo's default, else the tool's own choice. */
function defaultModel(runner: AgentRunner, repo: string | undefined, tool: ToolModel | undefined, prefix = 'Default'): { label: string; title?: string } {
  if (repo) return { label: `${prefix} (${repo}, repo setting)` }
  const home = TOOL_MODEL_HOME[runner]
  if (tool && home)
    return {
      label: `${prefix} (${tool.model}, as in ${home})`,
      title: `From ${tool.source}`,
    }
  if (home) return { label: `${prefix} (${home}'s own choice)` }
  return { label: `${prefix} (the tool's own choice)` }
}

const OTHER = '\u0000other'

/**
 * The model: for Claude Code a dropdown of its aliases with the tool's own
 * choice as the default, plus "Other model name…"; for other runners a name.
 */
function ModelPicker({
  id,
  runner,
  value,
  onChange,
  fallback,
  disabled,
}: {
  id: string
  runner: AgentRunner
  value: string
  onChange: (v: string) => void
  fallback: { label: string; title?: string }
  disabled?: boolean
}) {
  const known = CLAUDE_MODELS.some((m) => m.id === value)
  const [other, setOther] = useState(value !== '' && !known)
  if (runner !== 'claude')
    return <input id={id} value={value} disabled={disabled} onChange={(e) => onChange(e.target.value)} placeholder={fallback.label} title={fallback.title} />
  return (
    <span className="model-pick">
      <span className="native-select">
        <select
          id={id}
          value={other ? OTHER : value}
          disabled={disabled}
          title={value ? undefined : fallback.title}
          onChange={(e) => {
            const v = e.target.value
            setOther(v === OTHER)
            onChange(v === OTHER ? (known ? '' : value) : v)
          }}
        >
          <option value="">{fallback.label}</option>
          {CLAUDE_MODELS.map((m) => (
            <option key={m.id} value={m.id}>
              {m.label}
            </option>
          ))}
          <option value={OTHER}>Other model name…</option>
        </select>
      </span>
      {other && (
        <input
          aria-label="Model name"
          value={value}
          disabled={disabled}
          autoFocus
          onChange={(e) => onChange(e.target.value)}
          placeholder="full model name, as claude --model takes it"
        />
      )}
    </span>
  )
}

/** One setting, laid out like the editor's Settings: name, one line, control. */
function Setting({ id, name, help, children }: { id: string; name: string; help: ReactNode; children: ReactNode }) {
  return (
    <div className="set-row">
      <label className="set-name" htmlFor={id}>
        {name}
      </label>
      <div className="set-help">{help}</div>
      <div className="set-control">{children}</div>
    </div>
  )
}

function Select({
  id,
  value,
  disabled,
  onChange,
  children,
}: {
  id: string
  value: string
  disabled?: boolean
  onChange: (v: string) => void
  children: ReactNode
}) {
  return (
    <span className="native-select">
      <select id={id} value={value} disabled={disabled} onChange={(e) => onChange(e.target.value)}>
        {children}
      </select>
    </span>
  )
}

/** What the model setting means for a runner. */
function modelHelp(runner: AgentRunner, tool: ToolModel | undefined): ReactNode {
  if (runner === 'claude')
    return (
      <>
        Default runs the model you picked in Claude Code (<code>/model</code>, or the model menu of the Claude Code extension in VS Code and Cursor)
        {tool ? `, now ${tool.model} from ${tool.source}` : ''}.
      </>
    )
  if (runner === 'codex')
    return (
      <>
        Default runs the model in your Codex <code>config.toml</code>
        {tool ? `, now ${tool.model}` : ''}.
      </>
    )
  if (runner === 'cursor')
    return "Default is the Cursor agent CLI's own choice. Cursor's chat model menu can't be read by other tools, so name a model here to pin one."
  return 'Passed to your command as $AGENTVC_AGENT_MODEL.'
}

/** The start form's model: the workspace's, else the tool's own choice. */
function StartModel({
  workspace,
  runner,
  inherited,
  value,
  onChange,
}: {
  workspace: string
  runner: AgentRunner
  inherited?: string
  value: string
  onChange: (v: string) => void
}) {
  const data = useResource(`agent-settings:${workspace}`, () => api.agentSettings(workspace))
  const tool = data.data?.tool_models?.[runner]
  const fallback = inherited ? { label: `Workspace's (${inherited})` } : defaultModel(runner, data.data?.defaults.model, tool, "Workspace's")
  return <ModelPicker key={runner} id="agent-model" runner={runner} value={value} onChange={onChange} fallback={fallback} />
}

/** A workspace's agent settings: what agents started there inherit. */
export function AgentSettingsForm({ workspace }: { workspace: string }) {
  const steer = useCanSteer()
  const toast = useToast()
  const { refresh } = useLive()
  const data = useResource(`agent-settings:${workspace}`, () => api.agentSettings(workspace))
  const [draft, setDraft] = useState<AgentSettings | null>(null)
  const [busy, setBusy] = useState(false)
  if (data.error) return <ErrorNote error={data.error} />
  if (!data.data) return <Skeleton rows={2} />
  const { settings, defaults, machine_limit, tool_models } = data.data
  const s = draft ?? settings
  const set = (patch: Partial<AgentSettings>) => setDraft({ ...s, ...patch })
  const runner = s.runner ?? defaults.runner ?? 'claude'
  const tool = tool_models?.[runner]
  const dirty = draft !== null && JSON.stringify(draft) !== JSON.stringify(settings)
  const save = async (e: FormEvent) => {
    e.preventDefault()
    setBusy(true)
    try {
      await api.setAgentSettings(workspace, s)
      toast.ok('Saved. New agents in this workspace use these settings.', {
        undo: false,
      })
      setDraft(null)
    } catch (err) {
      toast.error(err)
    } finally {
      setBusy(false)
      refresh()
    }
  }
  const txt = (k: 'check' | 'command') => ({
    value: s[k] ?? '',
    onChange: (e: { target: { value: string } }) => set({ [k]: e.target.value || undefined }),
    disabled: !steer,
  })
  const id = (k: string) => `agent-${k}-${workspace}`
  return (
    <form className="ag-start ag-settings" id="agent-settings" onSubmit={(e) => void save(e)}>
      <h2 className="rv-h2">Agent settings</h2>
      <p className="muted ag-hint">
        Every agent started in this workspace uses these, unless you change them when you start it. Agents reuse your own Claude Code settings and MCP servers.
      </p>
      <div className="set-list">
        <Setting id={id('runner')} name="Runner" help="The coding agent that does the work.">
          <Select id={id('runner')} value={s.runner ?? ''} disabled={!steer} onChange={(v) => set({ runner: (v || undefined) as AgentRunner | undefined })}>
            <option value="">Default ({RUNNER_WORDS[defaults.runner ?? 'claude']})</option>
            {(Object.keys(RUNNER_WORDS) as AgentRunner[]).map((r) => (
              <option key={r} value={r}>
                {RUNNER_WORDS[r]}
              </option>
            ))}
          </Select>
        </Setting>
        {runner === 'command' && (
          <Setting
            id={id('command')}
            name="Command"
            help={
              <>
                Runs in the agent's copy with the prompt in <code>$AGENTVC_AGENT_PROMPT</code>.
              </>
            }
          >
            <input id={id('command')} {...txt('command')} placeholder={defaults.command ?? 'e.g. my-agent --yes'} />
          </Setting>
        )}
        <Setting id={id('model')} name="Model" help={modelHelp(runner, tool)}>
          <ModelPicker
            key={`${runner}:${settings.model ?? ''}`}
            id={id('model')}
            runner={runner}
            value={s.model ?? ''}
            disabled={!steer}
            onChange={(v) => set({ model: v || undefined })}
            fallback={defaultModel(runner, defaults.model, tool)}
          />
        </Setting>
        <Setting id={id('check')} name="Check command" help="Runs when an agent stops; a failure shows on the agent's page.">
          <input id={id('check')} {...txt('check')} placeholder={defaults.check ?? 'none, e.g. cargo test'} />
        </Setting>
        {runner === 'claude' && (
          <Setting
            id={id('permissions')}
            name="Permissions"
            help={
              <>
                What agents may do without asking you. <Link to={PERMISSIONS_LINK}>See what each level allows</Link>
              </>
            }
          >
            <Select
              id={id('permissions')}
              value={s.permissions ?? ''}
              disabled={!steer}
              onChange={(v) =>
                set({
                  permissions: (v || undefined) as AgentPermissions | undefined,
                })
              }
            >
              <option value="">Default ({PERMISSION_WORDS[defaults.permissions ?? DEFAULT_PERMISSIONS]})</option>
              {(Object.keys(PERMISSION_WORDS) as AgentPermissions[]).map((p) => (
                <option key={p} value={p}>
                  {PERMISSION_WORDS[p]}
                </option>
              ))}
            </Select>
          </Setting>
        )}
        {runner === 'claude' && (
          <Setting id={id('sandbox')} name="Sandbox commands" help="Commands the agent runs can write only inside its own copy.">
            <Select
              id={id('sandbox')}
              value={s.sandbox === undefined ? '' : s.sandbox ? 'on' : 'off'}
              disabled={!steer}
              onChange={(v) => set({ sandbox: v === '' ? undefined : v === 'on' })}
            >
              <option value="">Default ({defaults.sandbox ? 'on' : 'off'})</option>
              <option value="on">On</option>
              <option value="off">Off</option>
            </Select>
          </Setting>
        )}
        <Setting id={id('max')} name="At once" help={`Agents working at the same time in this workspace. This machine runs at most ${machine_limit}.`}>
          <input
            id={id('max')}
            className="set-num"
            type="number"
            min={1}
            value={s.max ?? ''}
            disabled={!steer}
            onChange={(e) =>
              set({
                max: e.target.value ? Math.max(1, Number(e.target.value)) : undefined,
              })
            }
            placeholder="no limit"
          />
        </Setting>
      </div>
      {steer && (
        <div className="ag-start-row">
          <button type="submit" className="btn btn-primary" disabled={busy || !dirty}>
            Save
          </button>
          {dirty && (
            <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => setDraft(null)}>
              Cancel
            </button>
          )}
        </div>
      )}
    </form>
  )
}

/** "Agents from here" on a branch page. */
export function AgentsFrom({ agents, workspace, branch }: { agents: Agent[]; workspace: string; branch: string }) {
  const here = agentsOn(agents, workspace, branch)
  if (!here.length) return null
  return (
    <section>
      <h2 className="rv-h2">Agents from here</h2>
      <div className="ag-lines">
        {here.map((a) => (
          <Link key={a.id} to={agentLink(a.id)} className="ag-line">
            <Dot a={a} />
            <span className="ag-line-title">{a.title}</span>
            <span className={`ag-meta ag-text-${agentState(a).tone}`}>{agentState(a).label}</span>
            <span className="ag-line-step muted">{a.question?.text ?? a.last_step ?? ''}</span>
          </Link>
        ))}
      </div>
    </section>
  )
}

/** `/review/a/:id`: one agent. */
export function AgentPage() {
  const { id = '' } = useParams()
  const steer = useCanSteer()
  const data = useResource(`agent:${id}`, () => api.agent(id))
  const [open, setOpen] = useState<string | null>(null)
  const target = `agent:${id}`
  const { threads } = useThreads(id ? target : null)
  if (data.error) return <div className="page rv"><ErrorNote error={data.error} /></div>
  if (!data.data) return <div className="page rv"><Skeleton rows={5} /></div>
  const { agent: a, steps } = data.data
  const q = a.question
  const files = a.result?.changes ?? a.result?.paths.map((path) => ({ path, kind: 'modified' as const })) ?? []
  const canReply = q?.kind === 'ask' || a.state === 'ready' || a.state === 'tests_failing' || a.state === 'failed' || a.state === 'stopped'
  return (
    <div className="page rv ag-page">
      <div>
        <Where a={a} />
        <h1 className="rv-title">{a.title}</h1>
        <p className="muted">
          {resultWords(a)} · {a.machine}
          {a.mode === 'new' && a.state !== 'discarded' ? (
            <>
              {' · branch '}
              <span className="mono">{a.branch}</span>
            </>
          ) : null}
          {a.landed ? ` · landed on ${a.landed.branch}${a.landed.outcome === 'queued' ? ' (waiting for that workspace)' : ''}` : ''}
          {(a.state !== 'discarded' || a.result?.commit) && (
            <>
              {' · '}
              <Link className="rv-code-link" to={codeLink(`agent:${a.id}`)} title="Browse every file of the agent's worktree, as it is now">
                Browse code
              </Link>
            </>
          )}
        </p>
      </div>

      <div className="ag-kv">
        <div><span>Time</span>{a.seconds > 0 ? duration(a.seconds) : '—'}</div>
        <div><span>Cost</span>{a.cost_usd > 0 ? money(a.cost_usd) : '—'}</div>
        <div><span>Changes</span>{a.result ? `${plural(a.result.files, 'file')}, +${a.result.insertions} −${a.result.deletions}` : '—'}</div>
        <div><span>Checks</span>{a.tests ? (a.tests.passed ? 'pass' : 'fail') : 'none'}</div>
        {a.runner === 'claude' && (
          <div>
            <span>Permissions</span>
            <Link to={PERMISSIONS_LINK} title={a.claude_mode ? `Claude Code mode: ${a.claude_mode}` : undefined}>
              {PERMISSION_WORDS[a.permissions ?? 'edits'].split(':')[0]}
              {a.sandbox ? ', sandboxed' : ''}
            </Link>
          </div>
        )}
      </div>

      {q && (
        <div className="ag-card ag-card-ask">
          {q.kind === 'ask' ? <p className="ag-q">{q.text}</p> : <pre className="ag-cmd">{q.text}</pre>}
          {steer && q.kind === 'approve' && <Actions a={a} />}
        </div>
      )}
      {!steer && <WatchOnly machine={a.machine} />}
      {steer && !q && <Actions a={a} />}
      {steer && canReply && <ReplyBox a={a} placeholder={q?.kind === 'ask' ? 'Answer the agent…' : 'Reply: what should it do next?'} />}
      {a.error && <p className="ag-err">{a.error}</p>}

      <section>
        <h2 className="rv-h2">Conversation</h2>
        <ul className="ag-msgs">
          {a.messages.map((m, i) => (
            <li key={i} className={`ag-msg ag-msg-${m.from}`}>
              <span className="ag-msg-who">{m.from === 'you' ? 'You' : 'Agent'}</span>
              <span className="ag-msg-text">{m.text}</span>
            </li>
          ))}
        </ul>
      </section>

      {a.tests && !a.tests.passed && (
        <section>
          <h2 className="rv-h2">
            Checks <span className="muted mono">· {a.tests.command}</span>
          </h2>
          <pre className="ag-cmd">{a.tests.tail.join('\n')}</pre>
        </section>
      )}

      <ReviewBar target={target} threads={threads} agent={a} readOnly={!steer} />
      <ThreadList
        target={target}
        threads={threads}
        onJump={(t) => {
          if (t.anchor) setOpen(t.anchor.path)
          scrollToThread(t.id)
        }}
        readOnly={!steer}
      />

      {files.length > 0 && a.result?.commit && (
        <section>
          <h2 className="rv-h2">
            Changes <span className="muted">· {plural(files.length, 'file')}</span>
          </h2>
          <div className="rv-files">
            {files.map((f) => (
              <div key={f.path} className="rv-file">
                <button type="button" className="rv-file-head" onClick={() => setOpen((o) => (o === f.path ? null : f.path))}>
                  <span className={`change change-${f.kind}`}>{f.kind}</span>
                  <span className="mono">{f.path}</span>
                  <span className="muted">{open === f.path ? '▾' : '▸'}</span>
                </button>
                {open === f.path && (
                  <DiffView
                    base={ref.git(a.start.commit)}
                    head={ref.git(a.result!.commit!)}
                    path={f.path}
                    kind={f.kind}
                    codeRef={`agent:${a.id}`}
                    localFile={a.state === 'landed' || a.state === 'discarded' ? undefined : joinPath(a.worktree, f.path)}
                    comments={{ target, threads, readOnly: !steer || a.state === 'landed' || a.state === 'discarded' }}
                  />
                )}
              </div>
            ))}
          </div>
        </section>
      )}

      <section>
        <h2 className="rv-h2">What it did</h2>
        {steps.length === 0 ? (
          <p className="muted">Nothing yet.</p>
        ) : (
          <ol className="ag-steps">
            {steps
              .filter((s) => s.kind !== 'you')
              .slice(-40)
              .map((s, i) => (
                <li key={i}>
                  <span className="muted">{new Date(s.at * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</span>
                  <span className={`ag-step ag-step-${s.kind}`}>{s.text.split('\n')[0]}</span>
                </li>
              ))}
          </ol>
        )}
      </section>
    </div>
  )
}
