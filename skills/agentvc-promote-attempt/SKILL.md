---
name: agentvc-promote-attempt
description: Promote an agentvc attempt as the winner of its task and, when asked, rebase it and open a GitHub PR. Use only when the user explicitly asks to promote, pick or ship a specific attempt, or says "/promote aN".
compatibility: Requires the agentvc CLI on PATH and a git repository with an agentvc space. Opening a PR also needs GITHUB_TOKEN and network access to GitHub.
metadata:
  author: agentvc
  version: "0.1.0"
---

# Promote an attempt

Promoting closes the attempt's open siblings as lost. Do it only on the user's
explicit instruction, naming the attempt. Never pick the winner yourself.

1. Run `agentvc promote aN --json`.
2. Report: which attempt won, which sibling attempts were closed, and the review
   link `http://localhost:7420/attempts/<N>`.
3. Tell the user how to ship it, and run these only if they ask:

   ```sh
   agentvc rebase aN   # brings it onto the latest main; a no-op if main has not moved
   agentvc pr aN       # exports one clean commit, pushes, opens or updates the PR
   ```

   If `rebase` fails with code `conflict`, list `error.paths` and stop. Offer
   either handing it back to the agent or `agentvc rebase aN --keep-conflicts`,
   which writes conflict markers into the view to fix by hand, then
   `agentvc snapshot aN`.
