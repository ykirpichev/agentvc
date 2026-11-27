// The words the review UI uses, in one place so every surface (web, VS Code,
// Cursor, Claude Code) says the same thing. The UI speaks git and GitHub;
// agentvc's own terms (attempt, promote, take) stay in the CLI and docs.

import type { AgentSummary, Attempt, ConflictHunk, PrBrief, Status } from '../api/types'
import { actorName, count, money } from '../lib/format'

export const WORDS = {
  version: 'version',
  versions: 'versions',
  choose: 'Use this',
  chosen: 'In use',
  notChosen: 'Not chosen',
  discard: 'Discard',
  updateFromMain: 'Update from main',
  rebaseOnto: 'Rebase onto…',
  moveOnto: 'Move onto…',
  updateFromParent: (name: string) => `Update from ${name}`,
  useIn: (name: string) => `Use in ${name}`,
  openPr: 'Open PR',
  undo: 'Undo',
  keepMain: 'Keep main',
  keepBoth: 'Keep both',
  keep: (name: string) => `Keep ${name}`,
  checkout: 'Check out',
  restack: 'Restack',
  resolve: 'Resolve conflicts',
  merge: 'Merge',
} as const

/** One line under each action saying what it does, in plain words. */
export const HELP = {
  choose: 'Ship this try. The other tries are set aside; Undo brings them back.',
  updateFromMain: (n: number) =>
    `Main has ${n} new commit${n === 1 ? '' : 's'}. Replays this version's changes on top; you see a preview first and settle any clashes here.`,
  rebaseOnto: 'Like Update from main, onto a main commit or another version you click in the branch graph. Onto a version stacks this one on top of it.',
  updateFromParent: (name: string) => `${name} changed since this was stacked on it. Replays this version on ${name}'s latest state; you see a preview first.`,
  openPrStacked: (name: string) => `Pushes this version as a branch and opens a pull request. It is stacked on ${name}, so the PR carries ${name}'s changes too.`,
  openPr: 'Pushes this version as a branch and opens a pull request on GitHub.',
  updatePr: 'Pushes this version again; the pull request updates.',
  take: (name: string) => `Copies this version's changes into ${name}, the version you chose.`,
  discard: 'Sets this version aside with a reason. It stays in history; Undo brings it back.',
  checkout: 'Switches that window (worktree) to this branch.',
  moveBranch: 'Moves this branch, with the branches above it, onto another branch or onto main. A clash stops the move here so you can settle it.',
  restack: 'Its parent changed. Replays this branch, and the branches above it, onto the parent\'s new tip.',
  resolve: 'Pick what each place should say, then continue.',
  merge: (n: number) =>
    n > 1
      ? `Merges the ${n} pull requests up to this one on GitHub, bottom first. After each, the rest moves onto main; it stops when the next one's checks are still running.`
      : 'Merges this pull request on GitHub and moves the branches above it onto main.',
} as const

/** A PR's state with a tone: `label` for sentences, `short` for the badge
 * next to `#N` in the narrow branch tree. */
export function prWords(
  state: string | null,
  st?: PrBrief | null,
): { label: string; short: string; tone: 'ok' | 'bad' | 'run' | 'win' | 'mute' } | null {
  if (state === 'merged') return { label: 'merged', short: 'merged', tone: 'win' }
  if (state === 'closed') return { label: 'closed', short: 'closed', tone: 'mute' }
  if (state === 'draft') return { label: 'draft', short: 'draft', tone: 'mute' }
  if (!st) return null
  if (st.checks === 'fail') return { label: 'checks fail', short: 'fails', tone: 'bad' }
  if (st.review === 'changes_requested') return { label: 'changes requested', short: 'changes', tone: 'bad' }
  if (st.mergeable === false) return { label: 'conflicts with its base', short: 'conflicts', tone: 'bad' }
  if (st.checks === 'pending') return { label: 'checks running', short: 'running', tone: 'run' }
  if (st.review === 'approved') return { label: 'approved', short: '✓ approved', tone: 'ok' }
  if (st.checks === 'pass') return { label: 'checks pass', short: '✓', tone: 'ok' }
  return null
}

/** A try's name: the first line of what it did, else who made it. */
export function tryName(a: Pick<Attempt, 'actor' | 'model' | 'prompt'>, summary?: string | null): string {
  const line = (summary ?? '').split('\n')[0]!.trim()
  if (line) return line.length > 80 ? `${line.slice(0, 77)}…` : line
  return versionName(a)
}

/** How a version is named: who made it, e.g. `claude · opus`. */
export function versionName(a: Pick<Attempt, 'actor' | 'model'>): string {
  const who = actorName(a.actor)
  return a.model ? `${who} · ${a.model}` : who
}

/** Status in plain words, from the reader's side. */
export function statusWords(s: Status): { label: string; tone: 'ok' | 'bad' | 'run' | 'win' | 'mute' } {
  switch (s) {
    case 'passed':
      return { label: 'Tests pass', tone: 'ok' }
    case 'failed':
      return { label: 'Tests fail', tone: 'bad' }
    case 'exploring':
      return { label: 'In progress', tone: 'run' }
    case 'promoted':
      return { label: 'In use', tone: 'win' }
    case 'abandoned':
      return { label: 'Not chosen', tone: 'mute' }
  }
}

export const shortSha = (c: string) => c.slice(0, 7)

/** The other side of a conflict hunk: `main` for a commit or main itself, else its label. */
export function theirsName(h: Pick<ConflictHunk, 'theirs_label'>): string {
  const l = h.theirs_label.trim()
  if (!l || /^[0-9a-f]{7,40}$/.test(l) || /^(main|master|origin\/)/.test(l)) return 'main'
  return l
}

/** A recorded test result in plain words. Stale means it ran before the latest changes. */
export function testsWords(t: { passed: boolean; stale?: boolean; summary?: string }): {
  label: string
  tone: 'ok' | 'bad' | 'mute'
  help: string
} {
  const detail = t.summary ? ` (${t.summary.replace(/^(pass|fail) · /, '')})` : ''
  if (t.stale)
    return {
      label: t.passed ? 'Tests passed before the latest changes' : 'Tests failed before the latest changes',
      tone: 'mute',
      help: `Recorded on an earlier state${detail}. Run them again: agentvc check`,
    }
  return t.passed
    ? { label: 'Tests pass', tone: 'ok', help: `Recorded on the latest state${detail}.` }
    : { label: 'Tests fail', tone: 'bad', help: `Recorded on the latest state${detail}.` }
}

/** Cost, tokens and models in one line, e.g. `$0.42 · 128k tokens · opus`. Empty when nothing is known. */
export function agentLine(a: AgentSummary | undefined, skipModel?: string | null): string {
  if (!a) return ''
  const parts: string[] = []
  if (a.cost_usd != null) parts.push(money(a.cost_usd))
  const tokens = (a.tokens_in ?? 0) + (a.tokens_out ?? 0)
  if (a.tokens_in != null || a.tokens_out != null) parts.push(`${count(tokens)} tokens`)
  const models = a.models.filter((m) => m !== skipModel)
  if (models.length) parts.push(models.join(', '))
  return parts.join(' · ')
}
