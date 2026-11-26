---
name: agentvc-attempt-tree
description: Show the agentvc attempt tree for the whole space or one task, with statuses and review links. Use when the user asks what attempts exist, what agents tried, what is running, or says "/attempts".
compatibility: Requires the agentvc CLI on PATH and a git repository with an agentvc space (`agentvc init`).
metadata:
  author: agentvc
  version: "0.1.0"
---

# Show the attempt tree

1. Run `agentvc log` (add `--task tN` if the user named a task). For what is
   running right now, also run `agentvc ps`.
2. Show the output as it is, in a code block. Do not reformat or summarize it
   unless the user asks.
3. Add the review link: `http://localhost:7420/tasks/<N>` for a task, otherwise
   `http://localhost:7420/` (or the base in `$AGENTVC_UI_URL`). If the UI is not
   running, say that `agentvcd` starts it.

Do not change anything. This skill only reads.
