---
description: Compare two agentvc attempts
argument-hint: "aX aY [aZ...]"
allowed-tools: Bash(${CLAUDE_PLUGIN_ROOT}/bin/agentvc-plugin:*), Read, Grep, Glob
---
!`${CLAUDE_PLUGIN_ROOT}/bin/agentvc-plugin slash compare $ARGUMENTS`

Show the user the list of files that differ (above) and the compare link. If it is short, summarize in two or three sentences how the attempts differ, reading the changed files in each attempt's view (paths above) if you need to. Do not change any files.
