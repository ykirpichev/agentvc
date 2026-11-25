---
name: agentvc-compare-attempts
description: Compare two or more agentvc attempts - which files differ, the patch, and a short summary of how the approaches differ. Use when the user asks to compare, diff or contrast attempts (aX aY), or says "/compare".
compatibility: Requires the agentvc CLI on PATH and a git repository with an agentvc space (`agentvc init`).
metadata:
  author: agentvc
  version: "0.1.0"
---

# Compare attempts

Input: two or more attempt ids, for example `a3 a5` or `a3 a5 a6`.

1. For each pair (first against each of the others), run
   `agentvc diff aX aY --stat`. For each attempt alone, `agentvc diff aN --stat`
   shows what it changed against the snapshot it was forked from.
2. Show the user the files that differ and the compare link,
   `http://localhost:7420/compare?a=3,5` (attempt numbers, comma separated).
3. If the diff is short, read it with `agentvc diff aX aY --patch` (or open the
   changed files in each attempt's view, from `agentvc path aN`) and summarize in
   two or three sentences how the approaches differ.
4. Do not change any files, and do not promote or abandon anything.

For a full scored review that recommends a winner, use the `attempt-review` skill
if it is installed.
