// The lines and dots of a stack, drawn like `gt log` behind a column of
// fixed-height rows: one straight line through a stack's dots, side stacks in
// their own column joining their parent's dot, the trunk as a square below.

import type { StackGraph } from '../core/stack'

export const LANE = 14

export function StackLines({ g, rowH, dot }: { g: StackGraph; rowH: number; dot: (row: number) => string }) {
  const x = (lane: number) => LANE / 2 + lane * LANE
  const y = (row: number) => row * rowH + rowH / 2
  const h = (g.rows.length + 1) * rowH
  return (
    <svg className="sl" width={g.lanes * LANE} height={h} aria-hidden>
      {g.edges.map((e, i) => {
        const x1 = x(e.fromLane)
        const x2 = x(e.toLane)
        const y1 = y(e.from)
        const y2 = y(e.to)
        const d = x1 === x2 ? `M${x1} ${y1}V${y2}` : `M${x1} ${y1}V${y2 - rowH * 0.75}C${x1} ${y2 - rowH * 0.3} ${x2} ${y2 - rowH * 0.45} ${x2} ${y2}`
        return <path key={i} d={d} className="sl-line" />
      })}
      {g.rows.map((r, i) => (
        <circle key={r.branch.name} cx={x(r.lane)} cy={y(i)} r={4.5} className={`sl-dot ${dot(i)}`} />
      ))}
      <rect x={x(0) - 5} y={y(g.rows.length) - 5} width={10} height={10} rx={2} className="sl-trunk" />
    </svg>
  )
}
