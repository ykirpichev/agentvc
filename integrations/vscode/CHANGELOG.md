# Changelog

## 0.2.0

- Branches panel is the shared review UI sidebar: one space, its workspaces
  and their stacks, agents under the branch they started from, Needs you.
- Start work in a workspace with no stack: its main row opens Start agent and
  New branch.
- Review comments on diffs, sent to agents and synced with GitHub PR reviews.
- Released together with agentvc 0.2.0 binaries; see docs/install.md.

## 0.1.0

First release.

- Attempts view: tasks → attempts nested by fork, with status, check result,
  snapshots, cost and live runs; changed files and snapshots per attempt.
- Compare in the multi-file diff editor: attempt vs fork point, two attempts,
  snapshot vs previous.
- Actions: promote, mark, abandon, fork, stop a run, open a PR, open the view,
  open in the web review UI.
- Status bar for windows opened on an attempt's view.
- Connects to a running `agentvcd`, starts one, or falls back to the CLI.
