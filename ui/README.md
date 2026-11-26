# Review UI

React + TypeScript (Vite) web app for comparing attempts, picking winners and
steering agents. It is served by the `agentvcd` daemon on localhost. See
[the design doc](../docs/design.md#human-control-and-review-ui).

## Run it

```sh
# 1. Serve a space (any directory inside it). Binds to 127.0.0.1 only.
cargo run -p agentvcd -- -C path/to/project        # http://localhost:7420

# 2a. Use the built UI from the daemon
cd ui && npm ci && npm run build                  # writes ui/dist, reload the page

# 2b. Or develop with live reload; /api is proxied to the daemon
cd ui && npm run dev                              # http://127.0.0.1:5173
AGENTVCD_URL=http://127.0.0.1:7421 npm run dev    # daemon on another port
```

Debug builds of `agentvcd` read `ui/dist` from disk at runtime; release builds
embed it, so build the UI before `cargo build --release`.

## The review screen

The sidebar holds the **branch graph**: main's recent commits down the left,
each task's versions (attempts) hung off the commit they are based on, forks
nested under the version they came from. Badges show what needs you: `↓N`
behind main, conflicts, the PR number. The pane on the right shows what is
selected:

- **What needs you** (`/review`): every task with its next step.
- **A task** (`/review/t/N`): one card per version with *Choose this version*,
  the next step as a pill and button (choose, resolve, update from main, open
  the PR), and the two best versions side by side, where *Use in …* copies a
  file from one version into the other.
- **A version** (`/review/v/N`): *Update from main*, *Rebase onto…*, *Open
  PR*, *Discard*, its conflicts (Keep main / Keep mine / Keep both / Edit),
  and its changed files.
- **A main commit** (`/review/c/SHA`): versions that started before it.

Drag a version onto a main commit (or use ↻ / ⤴ on its row) to rebase it: the
version pane first previews what merges cleanly, what auto-resolves and what
conflicts, and changes nothing until you press Rebase. Every change shows a
toast with Undo (`POST /api/undo`).

The UI speaks git and GitHub (version, choose, update from main, open PR);
`src/core/words.ts` keeps those words in one place, `src/core/graph.ts` lays
out the graph and `src/core/next.ts` decides the next step, so other surfaces
(the VS Code and Cursor extension, the Claude Code panel) can share them.

## Layout

| Path | What |
| --- | --- |
| `src/api/` | Typed client (`client.ts`, `types.ts` mirror the daemon's JSON), live event stream (`live.tsx`), `useResource` hook that refetches whenever the space changes |
| `src/keyboard/` | Shortcut framework: `useHotkeys` (keys, `mod+k`, sequences like `g i`, scoped dialogs) and `useListNavigation` (j/k/enter) |
| `src/theme/` | Light, dark and system themes, persisted per browser |
| `src/components/` | Shell, command palette, shortcut overlay, diff view, shared bits |
| `src/core/` | Shared by every surface: words, graph layout, next step, links |
| `src/pages/Review.tsx` | The review screen's panes (home, task, version, main commit) |
| `src/pages/` | Inbox, Tasks, Task (attempt tree), Compare (2 to 4 attempts side by side), Attempt (file diffs), Live |
| `src/lib/align.ts` | Patience line diff and the multi-way alignment behind Compare |
| `src/components/ReviewActions.tsx` | Pick winner, abandon and mark, with their dialogs |

## Keyboard

`?` lists every shortcut for the current page. Global: `g i` / `g t` / `g l`
to switch screens, `⌘K` or `/` to jump anywhere, `⇧T` to switch theme. Lists:
`j` / `k` to move, `enter` to open, `u` to go up a level.

Review: on a task, `space` selects attempts and `c` compares them (with none
selected, `c` compares the best open attempts); `e` folds an abandoned branch.
In Compare, `h` / `l` or `1`-`4` focus a column, `j` / `k` move between files,
`o` folds a file and `i` shows files identical in all attempts. Anywhere an
attempt is focused: `p` picks it as the winner, `x` abandons it with a reason,
`m p` / `m f` mark it passed or failed.

Compare renders a 5,000-line file in four columns (about 30,000 diff lines) in
roughly 40 to 50 ms from data to first paint: the first screen renders at
once, the rest follows as an interruptible transition, and off-screen chunks
skip layout with `content-visibility`. The `cmp-loaded` and `cmp-rendered`
performance marks bracket that time.

## Checks

`npm run typecheck`, `npm run lint` and `npm run build` run in CI.
