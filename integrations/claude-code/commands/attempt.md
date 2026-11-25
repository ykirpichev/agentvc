---
description: Fork a new agentvc attempt and switch into it
argument-hint: "[--from aN|sN] [--task tN] [what you will try]"
allowed-tools: Bash(${CLAUDE_PLUGIN_ROOT}/bin/agentvc-plugin:*)
---
## Fork result

!`${CLAUDE_PLUGIN_ROOT}/bin/agentvc-plugin slash attempt $ARGUMENTS`

## What to do

If the fork failed, show the error above and stop.

Otherwise, tell the user the new attempt id and its review link from the output above, in two short lines. Then switch into the attempt for the rest of this session:

- `cd` into the view path printed above before running any command.
- Read and edit files only by their absolute path inside that view. Never edit the main checkout or another attempt's view.
- When you use the agentvc MCP tools, pass this attempt's id explicitly (for example `attempt: "a7"`).
