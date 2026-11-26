# Space overview: experiments outside the daemon

`agentvc overview` summarizes a whole space as one JSON document: every task,
its attempt tree, each attempt's status and reason, the latest run and check,
and the PR the winner shipped as. Dashboards that cannot reach `agentvcd` render
it, starting with the **agentvc Experiments** artifact in the Claude project
(<https://claude.ai/artifact/4BvaSk6zJDTnGjcuBj6cPw>).

```sh
agentvc overview                         # one line per task
agentvc overview --json                  # {"ok": true, "overview": {...}}
agentvc overview --out overview.json     # the document alone, pretty-printed
curl localhost:7420/api/overview         # same document from the daemon
```

## Every PR in one place: `agentvc import`

Each cloud thread keeps its space in a collapsed comment on its PR (`agentvc pr`
writes it, see `crates/agentvc-core/src/bundle.rs`). `agentvc import --all-prs`
reads those comments and merges every space into the local one, so the review
UI, the VS Code extension, the side panel and `agentvc overview` show all PRs'
attempt trees together:

```sh
agentvc import --all-prs                 # every PR, open and closed
agentvc import --pr 87 --pr 90           # just these
```

- It creates the space first if the directory has none.
- A space saved on several PRs is taken from its newest comment only.
- Imported tasks and attempts get fresh ids. Each imported space's own root
  becomes a closed attempt (actor `import`) that its forks hang from, so your
  `a1` is never touched. `overview` gives each imported task a `source`
  (`owner/repo#N`).
- Running it again skips unchanged comments and replaces spaces whose comment
  changed, so it is safe to run on a schedule.
- Attempts whose patch was left out of the comment (it is capped at 64 KB) keep
  their history, status and reason but not their files.

## Publishing to the Claude project artifact

The artifact keeps one document per space in its database, in the `spaces`
collection, under the id `<owner>--<repo>` (for this repo `ykirpichev--agentvc`).
Only the artifact's owner and editors can write it; anyone it is shared with can
read it. Every open copy of the page updates live when the document changes.

1. Where the space lives, run `agentvc overview --out overview.json`. To show
   every PR without anyone's machine, do it in a fresh clone in the cloud:
   `agentvc import --all-prs && agentvc overview --out overview.json`.
2. In any Claude session that can see the file (a project thread, Claude Code on
   the same machine), ask: "push the agentvc overview". Claude reads the
   current document's `version`, then writes the file with the `ArtifactData`
   tool (`set`, collection `spaces`, doc id `<owner>--<repo>`, `file_path`
   `overview.json`, `if_version` = that version). Add `pushed_at` (unix seconds)
   if you want the page to show the push time rather than the generation time.

Running agents and checks show the state they had when the document was built.

## Schema `agentvc.overview/1`

```jsonc
{
  "schema": "agentvc.overview/1",
  "generated_at": 1795645427,            // unix seconds
  "space": {
    "name": "agentvc",                   // working directory name
    "repo": "ykirpichev/agentvc",        // from the origin remote, or null
    "head": "16f1d05…",                  // host repo HEAD, or null
    "counts": { "total": 14, "running": 2, "exploring": 2, "passed": 2,
                "failed": 2, "abandoned": 4, "promoted": 4 }
  },
  "tasks": [                             // newest activity first
    {
      "id": "t3", "goal": "…", "criteria": "…",   // criteria cut to 1200 chars
      "created_at": 0, "updated_at": 0,
      "state": "open",                   // open | done | idle
      "winner": "a8",                    // promoted attempt that is not "continued in …"
      "source": "ykirpichev/agentvc#90", // imported tasks only (agentvc import)
      "counts": { … },
      "attempts": [                      // creation order; parent links the tree
        {
          "id": "a8", "parent": "a6",
          "actor": "agent:claude", "model": "opus",
          "prompt": "…",                 // cut to 600 chars
          "status": "promoted",          // exploring | passed | failed | abandoned | promoted
          "reason": "lost to a8",        // or null
          "backend": "overlay",
          "created_at": 0, "updated_at": 0,
          "snapshots": 19,
          "changes": { "added": 3, "modified": 9, "removed": 0 },  // vs the fork point
          "run": {                       // latest run, or null
            "id": "r7", "state": "finished",   // running | lost | finished
            "command": "claude -p …", "check": "cargo test --workspace",
            "exit_code": 0, "check_exit_code": 0,
            "started_at": 0, "finished_at": 0, "duration_ms": 511000
          },
          "pr": { "url": "https://github.com/…/pull/48", "number": 48,
                  "branch": "agentvc/a8", "commit": "…" }        // or null
        }
      ]
    }
  ],
  "untasked": [ /* attempts with no task, except the root a1 */ ]
}
```

`state` is `open` while any attempt is exploring or running, `done` once one is
promoted and nothing is open, `idle` otherwise. Readers should check `schema`
and ignore fields they do not know; new fields may be added within version 1.
