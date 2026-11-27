# Review comments

Comment on any line of a diff, collect the comments into a review, and send it to
the agent working on that branch (#165). Threads follow their lines as the branch
moves on.

## In the review UI

On a branch page or an agent page, open a changed file and click a line number
(drag over several for a range). Write the comment and press **Add to review**
(Cmd/Ctrl+Enter), or **Comment now** to post it on its own. Removed lines take
comments too; they are on the old side.

Drafts collect in the bar at the top of the page: "3 comments not sent".

- **Send to agN**: shown when an agent works on the branch (the agent itself, or
  one revising the branch). An agent that has stopped starts its next turn with
  the review as its prompt; one that is working gets it at its next tool call.
- **Post comments** (or **Just post**): publish them without sending them to an
  agent. An agent that later works on the branch still sees them.
- **Discard**: delete your drafts.

Threads show under their line with their replies, **Reply** and **Resolve**. The
**Comments** list on the page lists every thread, open first; click one to jump to
it, or press `n` / `p`. A comment on the whole change (not on a line) goes through
**+ Comment** there. Sidebar rows show how many open comments a branch has.

## Where a thread sits

A thread keeps the path, the side, the lines it was written on and three lines of
context. Each time it is read, its lines are looked up in the file as it is now:
new commits, a restack onto a moved main and an agent's edits don't lose it. When
the lines are gone (deleted or changed), the thread is **outdated**: still listed,
shown above the file with the lines it was written on.

## States

`draft` (yours, not sent), `open`, `addressed` (the agent says it is done) and
`resolved` (a person closed it). Agents reply and mark threads addressed; only a
person resolves. Replying to an addressed thread opens it again.

## Agents

Agents started from the control center are told to address review comments and to
check `review_comments` before they finish. Three MCP tools work for any agent,
including ones started by hand in a workspace:

- `review_comments`: the threads on the agent's own work (it, and the branch
  checked out where it runs), with file, line, the quoted lines and replies.
- `reply_comment {id, text}`: reply as the agent.
- `resolve_comment {id}`: mark it addressed.

A review sent while an agent works reaches it at its next tool call: the
`agentvc hook post-tool-use` hook (and, for control-center agents, `agentvc comment
inbox`) hands it over as context, once. If the agent stops before that, the review
starts its next turn.

## CLI

```sh
agentvc comment add my-branch --path src/x.rs --line 10-12 --draft "Use a const"
agentvc comment add ag3 "Also add a test"          # on an agent; aN for a try
agentvc comment ls [my-branch|ag3] [--open] [--drafts] [--here]
agentvc comment reply c4 "Done in the last commit"
agentvc comment resolve c4                         # agents: --as-agent marks it addressed
agentvc comment send my-branch --agent             # your drafts, as one review
```

Every command takes `--json`. The API is `GET/POST /api/comments`,
`POST /api/comments/{id}/reply|resolve|reopen`, `DELETE /api/comments/{id}` (a
draft) and `POST /api/reviews/submit {target, to: "agent"|"none"}`.

Comments live with the space (`$AGENTVC_HOME/spaces/<id>/comments.json`), so every
checkout of the repo on this machine sees the same threads.
