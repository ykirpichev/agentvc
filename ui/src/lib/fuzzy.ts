// Fuzzy path matching for go-to-file: every query character in order, with
// bonuses for matches at the start of a path segment or word and for runs,
// and a preference for matches in the file name.

export interface FuzzyHit {
  path: string
  score: number
  /** Indexes into `path` of the matched characters. */
  at: number[]
}

const isBoundary = (p: string, i: number) => i === 0 || '/_-. '.includes(p[i - 1]!) || (/[a-z]/.test(p[i - 1]!) && /[A-Z]/.test(p[i]!))

export function fuzzyMatch(path: string, query: string): FuzzyHit | null {
  const q = query.replace(/\s+/g, '').toLowerCase()
  if (!q) return { path, score: 0, at: [] }
  const lower = path.toLowerCase()
  const nameStart = path.lastIndexOf('/') + 1
  // Greedy from the end of the name backwards gives tighter matches in the
  // file name; fall back to a forward scan.
  const at: number[] = []
  let qi = 0
  for (let i = 0; i < lower.length && qi < q.length; i++) {
    if (lower[i] === q[qi]) {
      at.push(i)
      qi++
    }
  }
  if (qi < q.length) return null
  // Try to move the match into the file name when the tail fits there.
  const name = lower.slice(nameStart)
  let ni = 0
  const nameAt: number[] = []
  for (let i = 0; i < name.length && ni < q.length; i++) {
    if (name[i] === q[ni]) {
      nameAt.push(nameStart + i)
      ni++
    }
  }
  const best = ni === q.length ? nameAt : at
  let score = 0
  for (let k = 0; k < best.length; k++) {
    const i = best[k]!
    score += 1
    if (isBoundary(path, i)) score += 4
    if (k > 0 && best[k - 1] === i - 1) score += 3
    if (i >= nameStart) score += 2
  }
  if (name === q || name.startsWith(q)) score += 20
  score -= path.length / 100
  return { path, score, at: best }
}

/** The best `limit` matches, best first. */
export function fuzzyFilter(paths: string[], query: string, limit = 50): FuzzyHit[] {
  if (!query.trim()) return paths.slice(0, limit).map((path) => ({ path, score: 0, at: [] }))
  const hits: FuzzyHit[] = []
  for (const p of paths) {
    const h = fuzzyMatch(p, query)
    if (h) hits.push(h)
  }
  hits.sort((a, b) => b.score - a.score || a.path.length - b.path.length || a.path.localeCompare(b.path))
  return hits.slice(0, limit)
}
