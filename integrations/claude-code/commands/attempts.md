---
description: Show the agentvc attempt tree
argument-hint: "[tN]"
allowed-tools: Bash(${CLAUDE_PLUGIN_ROOT}/bin/agentvc-plugin:*)
---
!`${CLAUDE_PLUGIN_ROOT}/bin/agentvc-plugin slash attempts $ARGUMENTS`

Show the user the attempt tree above as it is, in a code block, followed by the review link. Do not add commentary unless they ask.
