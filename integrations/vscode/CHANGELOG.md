# Changelog

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
