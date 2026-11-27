// The words the review UI uses, in one place so every surface (web, VS Code,
// Cursor, Claude Code) says the same thing. The UI speaks git and GitHub;
// agentvc's own terms (attempt, promote, take) stay in the CLI and docs.

import type { AgentSummary, Attempt, ConflictHunk, Status } from '../api/types'
import { actorName, count, money } from '../lib/format'

export const WORDS = {
  version: 'version',
  versions: 'versions',
  choose: 'Choose this version',
  chosen: 'Chosen',
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
} as const

/** One line under each action saying what it does, in plain words. */
export const HELP = {
  choose: 'Pick this as the version to ship. The task\'s other versions are set aside; Undo brings them back.',
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
} as const

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
      return { label: 'Chosen', tone: 'win' }
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
