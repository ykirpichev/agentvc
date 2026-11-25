import type { AttemptStats } from '../api/types'
import { duration, money, plural } from '../lib/format'
import { Icon } from './Icon'

/** Checks, cost, active time and snapshot count as a row of chips. */
export function StatChips({
  stats,
  lines,
  files,
  className,
}: {
  stats: AttemptStats
  lines?: { added: number; removed: number }
  files?: number
  className?: string
}) {
  const check = stats.check
  return (
    <div className={`stat-chips ${className ?? ''}`}>
      <span
        className={`stat-chip ${check ? (check.passed ? 'chip-ok' : 'chip-bad') : 'chip-none'}`}
        title={check ? `Check exited ${check.exit_code ?? '?'} (s${check.snapshot})` : 'No check has run'}
      >
        <Icon name="flask" size={13} />
        {check ? (check.passed ? 'Checks pass' : 'Checks fail') : 'No checks'}
      </span>
      {lines && (
        <span className="stat-chip" title="Lines added and removed against the common base">
          <span className="add">+{lines.added}</span>
          <span className="del">−{lines.removed}</span>
          {files !== undefined && <span className="muted">{plural(files, 'file')}</span>}
        </span>
      )}
      <span className="stat-chip" title="Reported cost of model calls">
        <Icon name="coin" size={13} />
        {stats.cost_usd != null ? money(stats.cost_usd) : <span className="muted">no cost data</span>}
      </span>
      <span className="stat-chip" title={stats.run_ms != null ? `Runs took ${duration(stats.run_ms / 1000)}` : 'From fork to the latest snapshot'}>
        <Icon name="clock" size={13} />
        {duration(stats.run_ms != null ? stats.run_ms / 1000 : stats.duration_s)}
      </span>
      <span className="stat-chip muted" title="Snapshots recorded">
        <Icon name="camera" size={13} />
        {stats.snapshots}
      </span>
    </div>
  )
}
