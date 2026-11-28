# Changelog

## Unreleased

- Sends agentvcd's token (`$AGENTVC_HOME/agentvcd.token`) with actions;
  agentvcd now refuses changes without it (#199).
- The review panel takes the editor's own look: its font, inputs, dropdowns,
  buttons and focus ring follow the current VS Code or Cursor theme.
- Agent settings read like the editor's Settings, and the model is a dropdown
  of Claude Code models whose default is the one you picked in Claude Code.
- In diffs, *Open diff* shows the change in the editor's own diff view and
  *Open file* opens the file in the editor (the checked-out file when it is on
  this machine), instead of agentvc's file viewer. The browser keeps *View file*.

## 0.2.1

- The panel sets itself up: *Install agentvc on <host>* when the CLI is
  missing (the remote host in a Remote-SSH window), *Set up agentvc here* when
  the folder has no space, instead of a dead end asking for `agentvc init`.
- The CLI and daemon are found in `~/.local/bin` and other install folders
  even when they are not on the editor's PATH; agentvcd is also looked for next
  to the CLI.
- Runs on the workspace side in remote windows (`extensionKind: workspace`).

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
