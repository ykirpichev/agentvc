// How a space's workspaces are listed, the same in the sidebar and on the
// home page: this window's first, then the ones where something is going on,
// and the idle rest folded into one line. Worktrees that coding tools make
// for their own agents (Claude Code's `.claude/worktrees/agent-…`) are not
// workspaces of their own: they sit under the workspace they came from.

import type { Agent, WorkspaceInfo } from '../api/types'
import { plural } from '../lib/format'

/** Fold the idle workspaces into one line once there are at least this many. */
export const FOLD_AT = 3

export interface Placed {
  w: WorkspaceInfo
  /** Its tools' worktrees with something going on, listed under it. */
  tools: WorkspaceInfo[]
  /** Its tools' idle worktrees, folded into one line under it. */
  idleTools: WorkspaceInfo[]
}

export interface Arranged {
  /** This window's workspace, then the ones with work going on. */
  shown: Placed[]
  /** Nothing going on: shown folded as "N idle workspaces". */
  idle: Placed[]
  /** Workspaces that are not a tool's worktree. */
  count: number
  /** Tools' worktrees. */
  toolCount: number
}

const openAgent = (a: Agent) => a.state !== 'landed' && a.state !== 'discarded'

/** Something is going on in `w`: unmerged branches, an agent, edits waiting. */
export function isActive(w: WorkspaceInfo, agents: Agent[] = []): boolean {
  return (
    w.branches.some((b) => b.pr?.state !== 'merged') ||
    w.pending !== null ||
    w.state === 'agent_running' ||
    w.requests.some((r) => r.status === 'queued') ||
    agents.some((a) => a.workspace === w.id && openAgent(a))
  )
}

/**
 * `selected`: the workspace whose page is open; it is shown even when idle,
 * so what you clicked stays in view.
 */
export function arrangeWorkspaces(list: WorkspaceInfo[], here: WorkspaceInfo | undefined, agents: Agent[] = [], selected?: string): Arranged {
  const ids = new Set(list.map((w) => w.id))
  const nested = (w: WorkspaceInfo) => !!w.nested_in && ids.has(w.nested_in) && w.nested_in !== w.id && w !== here
  const kids = new Map<string, WorkspaceInfo[]>()
  for (const w of list) {
    if (!nested(w)) continue
    kids.set(w.nested_in!, [...(kids.get(w.nested_in!) ?? []), w])
  }
  const active = (w: WorkspaceInfo) => w.id === selected || isActive(w, agents)
  const placed: Placed[] = list
    .filter((w) => !nested(w))
    .map((w) => {
      const ks = kids.get(w.id) ?? []
      return { w, tools: ks.filter(active), idleTools: ks.filter((k) => !active(k)) }
    })
  const busy = (p: Placed) => p.w === here || active(p.w) || p.tools.length > 0
  const rank = (p: Placed) => (p.w === here ? 0 : active(p.w) ? 1 : 2)
  let shown = placed.filter(busy).sort((a, b) => rank(a) - rank(b))
  let idle = placed.filter((p) => !busy(p))
  // A fold line for one or two workspaces hides more than it saves.
  if (idle.length < FOLD_AT) {
    shown = [...shown, ...idle]
    idle = []
  }
  // With nothing going on anywhere, still show one workspace to start from.
  if (shown.length === 0 && idle.length > 0) {
    shown = [idle[0]!]
    idle = idle.slice(1)
  }
  return { shown, idle, count: placed.length, toolCount: list.length - placed.length }
}

/** "3 Claude Code worktrees", or "3 agent worktrees" when tools differ. */
export function toolWords(ws: WorkspaceInfo[], adjective = ''): string {
  const tools = new Set(ws.map((w) => w.tool ?? 'agent'))
  const tool = tools.size === 1 ? [...tools][0]! : 'agent'
  return plural(ws.length, `${adjective ? `${adjective} ` : ''}${tool} worktree`)
}
