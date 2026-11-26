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
  useIn: (name: string) => `Use in ${name}`,
  openPr: 'Open PR',
  undo: 'Undo',
  keepMain: 'Keep main',
  keepBoth: 'Keep both',
  keep: (name: string) => `Keep ${name}`,
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
