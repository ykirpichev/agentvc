---
name: agentvc-fork-attempt
description: Fork a new agentvc attempt and switch the session into its isolated view. Use when the user asks to start a new attempt, try another approach, branch off an attempt, or says "/attempt".
compatibility: Requires the agentvc CLI on PATH and a git repository with an agentvc space (`agentvc init`).
metadata:
  author: agentvc
  version: "0.1.0"
---

# Fork an attempt

1. Fork. Pass what the user said they will try as the prompt, and `--from` or
   `--task` only if they named one:

   ```sh
   agentvc fork --actor agent:<your-name> --prompt "<what you will try>" [--from aN|sN] [--task tN] --json
   ```

   Without `--from` it forks from the attempt of the current directory. The
   JSON result has the new attempt id and `view_path`.

2. If it failed, show the error `code` and `message` and stop. `not_found` with
   no space means the repository needs `agentvc init` first; say so.

3. Tell the user, in two short lines, the new attempt id and its review link,
   `http://localhost:7420/attempts/<N>` (or `$AGENTVC_UI_URL`).

4. Switch into the attempt for the rest of the session:
   - `cd` into `view_path` before running any command.
   - Read and edit files only by absolute paths inside that view. Never edit the
     main checkout or another attempt's view.
   - When you call agentvc commands or MCP tools, pass this attempt's id explicitly.
