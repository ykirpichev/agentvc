---
description: Promote an agentvc attempt as the winner of its task
argument-hint: "aN"
allowed-tools: Bash(${CLAUDE_PLUGIN_ROOT}/bin/agentvc-plugin:*)
disable-model-invocation: true
---
!`${CLAUDE_PLUGIN_ROOT}/bin/agentvc-plugin slash promote $ARGUMENTS`

Report the result above to the user: which attempt won, which sibling attempts were closed, the review link, and the command to open a PR. Do not run `agentvc pr` unless the user asks.
