import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import type { DaemonEvent } from './types'

export type Connection = 'connecting' | 'live' | 'offline'

export interface ReceivedEvent {
  seq: number
  at: number
  event: Exclude<DaemonEvent, { type: 'hello' } | { type: 'resync' }>
}

interface LiveState {
  connection: Connection
  /** Bumped on every change in the space; data hooks refetch when it moves. */
  revision: number
  /** Newest first, capped. */
  recent: ReceivedEvent[]
  /** Refetch everything now, e.g. right after this client wrote. */
  refresh: () => void
}

const LiveContext = createContext<LiveState>({
  connection: 'connecting',
  revision: 0,
  recent: [],
  refresh: () => {},
})

const MAX_RECENT = 200

/** Keeps one WebSocket to `/api/events` open, reconnecting with backoff. */
export function LiveProvider({ children }: { children: ReactNode }) {
  const [connection, setConnection] = useState<Connection>('connecting')
  const [revision, setRevision] = useState(0)
  const [recent, setRecent] = useState<ReceivedEvent[]>([])
  const seq = useRef(0)

  useEffect(() => {
    let socket: WebSocket | null = null
    let retry: number | undefined
    let attempt = 0
    let stopped = false

    const connect = () => {
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
      socket = new WebSocket(`${proto}//${location.host}/api/events`)
      socket.onmessage = (msg) => {
        let ev: DaemonEvent
        try {
          ev = JSON.parse(String(msg.data)) as DaemonEvent
        } catch {
          return
        }
        if (ev.type === 'hello') {
          attempt = 0
          setConnection('live')
          // We may have missed changes while disconnected.
          setRevision((r) => r + 1)
          return
        }
        setRevision((r) => r + 1)
        if (ev.type === 'resync') return
        const entry = { seq: ++seq.current, at: Date.now(), event: ev }
        setRecent((xs) => [entry, ...xs].slice(0, MAX_RECENT))
      }
      socket.onclose = () => {
        if (stopped) return
        setConnection('offline')
        const delay = Math.min(10_000, 500 * 2 ** attempt++)
        retry = window.setTimeout(connect, delay)
      }
    }
    connect()
    return () => {
      stopped = true
      window.clearTimeout(retry)
      socket?.close()
    }
  }, [])

  const value = useMemo(
    () => ({ connection, revision, recent, refresh: () => setRevision((r) => r + 1) }),
    [connection, revision, recent],
  )
  return <LiveContext.Provider value={value}>{children}</LiveContext.Provider>
}

// eslint-disable-next-line react-refresh/only-export-components
export function useLive(): LiveState {
  return useContext(LiveContext)
}
