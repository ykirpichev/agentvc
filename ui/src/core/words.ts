// The words the review UI uses, in one place so every surface (web, VS Code,
// Cursor, Claude Code) says the same thing. The UI speaks git and GitHub;
// agentvc's own terms (attempt, promote, take) stay in the CLI and docs.

import type { Attempt, Status } from '../api/types'
import { actorName } from '../lib/format'

export const WORDS = {
  version: 'version',
  versions: 'versions',
  choose: 'Choose this version',
  chosen: 'Chosen',
  notChosen: 'Not chosen',
  discard: 'Discard',
  updateFromMain: 'Update from main',
  rebaseOnto: 'Rebase onto…',
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
