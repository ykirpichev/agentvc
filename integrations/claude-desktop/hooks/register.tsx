// agentvc panel: a pane beside the chat that shows the local agentvc space.
// Tasks, each task's attempt tree with outcomes and abandon reasons, and the
// agents running right now. It polls agentvcd's JSON API through Claude Code
// (`$.http.fetch`, never a browser), so the daemon's localhost guard holds.

import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { AgentvcBoard, AgentvcLive } from '../types'
import {
  EMPTY_BOARD,
  ago,
  isActive,
  isSettled,
  loadBoard,
  money,
  statusLook,
  taskSummary,
  taskTree,
} from './model'

const PANE = 'agentvc'
const TITLE = 'agentvc'
/** Redraw at least this often so the ages in the pane stay true. */
const REFRESH_AGES_S = 30

// Change a shape tag when that value's fields change: it declines what an
// earlier version stored, which outlives a reload.
const boardAtom = atom({ plugin: 'agentvc-panel', key: 'board' } as const, EMPTY_BOARD, {
  shape: 'board-1',
})
const expandedAtom = atom({ plugin: 'agentvc-panel', key: 'expanded' } as const, [] as number[], {
  shape: 'expanded-1',
})
const showDoneAtom = atom({ plugin: 'agentvc-panel', key: 'showDone' } as const, false)

type $ = EngineInterface

// Read from the options at each load; module variables start over on a
// reload, while the board itself lives in $.state.
let base = 'http://127.0.0.1:7420'
let pollMs = 2000
let autoOpen = true
let inFlight = false
let lastJson = ''
let lastWriteS = 0
let hasOpened = false

function nowS(): number {
  return Math.floor(Date.now() / 1000)
}

function errorMessage(text: string, status: number): string {
  try {
    return JSON.parse(text)?.error?.message ?? `HTTP ${status}`
  } catch {
    return `HTTP ${status}`
  }
}

async function getJson($: $, path: string): Promise<unknown> {
  const res = await $.http.fetch(base + path, { headers: { accept: 'application/json' } })
  if (!res.ok) throw new Error(`${path}: ${errorMessage(res.text, res.status)}`)
  return JSON.parse(res.text) as unknown
}

async function poll($: $): Promise<void> {
  if (inFlight) return
  inFlight = true
  try {
    const expanded = await read($, expandedAtom)
    let board: AgentvcBoard
    try {
      board = await loadBoard(path => getJson($, path), expanded, nowS())
    } catch (err) {
      const prev = await read($, boardAtom)
      board = {
        ...prev,
        isReachable: false,
        error: err instanceof Error ? err.message : String(err),
        fetchedAt: nowS(),
      }
    }
    const json = JSON.stringify({ ...board, fetchedAt: 0 })
    if (json !== lastJson || nowS() - lastWriteS >= REFRESH_AGES_S) {
      lastJson = json
      lastWriteS = nowS()
      await update($, boardAtom, () => board)
      const running = board.live.filter(l => l.isRunning).length
      $.ui.status(board.isReachable && running > 0 ? `agentvc: ${running} running` : undefined)
    }
    if (board.isReachable && autoOpen && !hasOpened) {
      hasOpened = true
      void $.ui.open({ id: PANE, title: TITLE })
    }
  } finally {
    inFlight = false
  }
}

async function openUrl($: $, url: string): Promise<void> {
  const { sysname } = await $.session.machine()
  const argv =
    sysname === 'Darwin'
      ? ['open', url]
      : /windows|mingw|msys|cygwin/i.test(sysname)
        ? ['cmd', '/c', 'start', '', url]
        : ['xdg-open', url]
  try {
    const res = await $.process.run(argv, { timeoutMs: 5000 })
    if (res.exitCode === 0) return
  } catch {
    // fall through to the clipboard
  }
  await $.ui.copy({ text: url })
  $.ui.toast(`Copied ${url}`)
}

async function stop($: $, attempt: number): Promise<void> {
  const res = await $.http.fetch(`${base}/api/attempts/a${attempt}/stop`, { method: 'POST' })
  $.ui.toast(
    res.ok
      ? `Asked a${attempt} to stop`
      : `Could not stop a${attempt}: ${errorMessage(res.text, res.status)}`,
  )
  await poll($)
}

async function toggleTask($: $, id: number): Promise<void> {
  await update($, expandedAtom, list =>
    list.includes(id) ? list.filter(x => x !== id) : [...list, id],
  )
  await poll($)
}

async function toggleDone($: $): Promise<void> {
  await update($, showDoneAtom, v => !v)
}

export const register: Register = (on, options) => {
  base = String(options.url ?? base).replace(/\/+$/, '')
  pollMs = Math.max(1, Number(options.pollSeconds ?? 2)) * 1000
  autoOpen = options.autoOpen !== false

  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'agentvc',
      description: 'Show the agentvc attempt tree and running agents in a side pane',
    })
    $.clock.every(pollMs, () => void poll($))
    void poll($)
    return next(e)
  })

  on('command.run', { command: 'agentvc' }, async $ => {
    hasOpened = true
    const opened = await $.ui.open({ id: PANE, title: TITLE, focus: true })
    void poll($)
    return { text: opened.isPlaced ? 'agentvc pane opened.' : 'agentvc pane is waiting for room.' }
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Text, Button } = $.ui.resolve(e)
    const board = await read($, boardAtom)
    const expanded = await read($, expandedAtom)
    const showDone = await read($, showDoneAtom)
    const now = nowS()

    const liveByAttempt = new Map<number, AgentvcLive>(board.live.map(l => [l.attempt, l]))
    const active = board.live.filter(l => isActive(l, now))
    const running = board.live.filter(l => l.isRunning)
    const needsReview = new Set(board.inbox)

    // The minimized form: one glance line.
    if (e.props.placement === 'widget') {
      if (!board.isReachable) return <Text dimColor>agentvc: daemon not running</Text>
      return (
        <Box flexDirection="column">
          <Text bold>agentvc · {board.space}</Text>
          <Text color={running.length > 0 ? 'claude' : 'inactive'}>
            {running.length} running · {active.length} active
          </Text>
          <Text color={needsReview.size > 0 ? 'warning' : 'inactive'}>
            {needsReview.size} need review
          </Text>
        </Box>
      )
    }

    if (!board.isReachable && board.fetchedAt > 0 && board.tasks.length === 0) {
      return (
        <Box flexDirection="column" gap={1}>
          <Text color="warning">agentvcd is not answering at {base}</Text>
          <Text dimColor>Start it in your repository:</Text>
          <Text mono>agentvcd</Text>
          {board.error && (
            <Text dimColor size="caption" wrap="wrap">
              {board.error}
            </Text>
          )}
          <Button key="retry" onPress={() => poll($)}>
            Retry
          </Button>
        </Box>
      )
    }
    if (board.fetchedAt === 0) return <Text dimColor>Connecting to agentvcd…</Text>

    const open = board.tasks.filter(t => !isSettled(t))
    const settled = board.tasks.filter(t => isSettled(t))
    const shown = showDone ? [...open, ...settled] : open

    const taskBlock = (taskId: number) => {
      const task = board.tasks.find(t => t.id === taskId)
      if (!task) return null
      const isOpen = expanded.includes(task.id)
      const taskRunning = board.attempts.filter(
        a => a.task_id === task.id && liveByAttempt.get(a.id)?.isRunning,
      ).length
      const winner = board.attempts.find(a => a.task_id === task.id && a.status === 'promoted')
      return (
        <Box key={`task-${task.id}`} flexDirection="column">
          <Button key={`t${task.id}`} plain onPress={() => toggleTask($, task.id)}>
            {isOpen ? '▾ ' : '▸ '}
            <Text bold>t{task.id}</Text> {task.goal}
          </Button>
          <Box paddingLeft={2} flexDirection="row" gap={1}>
            <Text dimColor size="caption">
              {taskSummary(task, taskRunning)}
            </Text>
            {winner && (
              <Text color="success" size="caption">
                ★ a{winner.id}
              </Text>
            )}
            {needsReview.has(task.id) && !winner && (
              <Text color="warning" size="caption">
                needs review
              </Text>
            )}
          </Box>
          {isOpen && (
            <Box flexDirection="column" paddingLeft={2} marginTop={1}>
              {taskTree(board.attempts, task.id).map(({ attempt: a, prefix, pad }) => {
                const live = liveByAttempt.get(a.id)
                const look = statusLook(a.status, live?.isRunning === true)
                const st = board.stats[`a${a.id}`]
                const note =
                  a.status === 'abandoned' || a.status === 'failed'
                    ? (a.reason ?? st?.summary)
                    : (st?.summary ?? (live?.step ? `step: ${live.step}` : null))
                const meta = [a.model, money(st?.cost), live?.files ? `${live.files} files` : null]
                  .filter(Boolean)
                  .join(' · ')
                return (
                  <Box key={`a${a.id}`} flexDirection="column">
                    <Box flexDirection="row">
                      <Text dimColor mono>
                        {prefix}
                      </Text>
                      <Text color={look.color} bold={a.status === 'promoted'}>
                        {look.glyph}
                      </Text>
                      <Text> </Text>
                      <Button key={`open-a${a.id}`} plain dimColor={look.isDim} onPress={() => openUrl($, `${base}/attempts/${a.id}`)}>
                        <Text bold strikethrough={a.status === 'abandoned'}>
                          a{a.id}
                        </Text>{' '}
                        <Text color={look.color}>{look.label}</Text>
                        {meta ? <Text dimColor> {meta}</Text> : ''}
                      </Button>
                    </Box>
                    {note && (
                      <Box flexDirection="row">
                        <Text dimColor mono>
                          {pad}
                        </Text>
                        <Text dimColor italic={a.status === 'abandoned'} size="caption" wrap="end">
                          {note}
                        </Text>
                      </Box>
                    )}
                  </Box>
                )
              })}
              <Box flexDirection="row" gap={1} marginTop={1}>
                <Button key={`review-t${task.id}`} dimColor onPress={() => openUrl($, `${base}/tasks/${task.id}`)}>
                  Review in browser
                </Button>
              </Box>
            </Box>
          )}
        </Box>
      )
    }

    return (
      <Box flexDirection="column" gap={1}>
        <Box flexDirection="row" justifyContent="space-between">
          <Text bold>{board.space}</Text>
          <Text color={board.isReachable ? 'success' : 'warning'} size="caption">
            {board.isReachable ? '● live' : '○ daemon not answering, showing last state'}
          </Text>
        </Box>

        <Box flexDirection="column">
          <Text bold dimColor size="caption">
            RUNNING · {running.length}
          </Text>
          {active.length === 0 && <Text dimColor>No agents working right now.</Text>}
          {active.map(l => (
            <Box key={`live-${l.attempt}`} flexDirection="column" marginTop={1}>
              <Box flexDirection="row" gap={1}>
                <Text color={l.isRunning ? 'claude' : 'suggestion'}>{l.isRunning ? '●' : '○'}</Text>
                <Text bold>a{l.attempt}</Text>
                <Text wrap="end">{l.goal ?? 'no task'}</Text>
              </Box>
              <Box flexDirection="row" paddingLeft={2} gap={1}>
                <Text dimColor size="caption">
                  {[
                    l.model,
                    l.step ? `step: ${l.step}` : null,
                    l.files > 0 ? `${l.files} files` : null,
                    l.overlaps > 0 ? `${l.overlaps} overlap` : null,
                    `${ago(now - l.lastActivity)} ago`,
                  ]
                    .filter(Boolean)
                    .join(' · ')}
                </Text>
                {l.isRunning && (
                  <Button key={`stop-a${l.attempt}`} dimColor onPress={() => stop($, l.attempt)}>
                    Stop
                  </Button>
                )}
              </Box>
              {l.logTail && (
                <Box paddingLeft={2}>
                  <Text dimColor mono size="caption" wrap="end">
                    {l.logTail}
                  </Text>
                </Box>
              )}
            </Box>
          ))}
        </Box>

        <Box flexDirection="column" gap={1}>
          <Text bold dimColor size="caption">
            TASKS · {open.length} open
          </Text>
          {board.tasks.length === 0 && (
            <Text dimColor>No tasks yet. Start one with `agentvc task new "goal"`.</Text>
          )}
          {shown.map(t => taskBlock(t.id))}
          {settled.length > 0 && (
            <Button key="toggle-done" plain dimColor onPress={() => toggleDone($)}>
              {showDone ? `Hide ${settled.length} settled` : `Show ${settled.length} settled`}
            </Button>
          )}
        </Box>

        <Box flexDirection="row" gap={1}>
          <Button key="open-ui" dimColor onPress={() => openUrl($, `${base}/inbox`)}>
            Open review UI
          </Button>
          <Button key="refresh" dimColor onPress={() => poll($)}>
            Refresh
          </Button>
        </Box>
      </Box>
    )
  })
}
