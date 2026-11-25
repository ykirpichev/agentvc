# agentvc agent skills

Standalone [Agent Skills](https://agentskills.io/specification) (`SKILL.md`
packages) for agentvc. They work in any agent that supports the standard,
such as Claude Code, Cursor, Codex and Copilot, because they only tell the agent
which `agentvc` and `git` commands to run. They ship no scripts or binaries.

| Skill | Needs agentvc | What it does |
| --- | --- | --- |
| `agentvc-attempt-workflow` | yes | Work inside an agentvc space: fork before risky approaches, record outcomes, never edit outside your view. Same rules as the Claude Code plugin's skill, CLI-first. |
| `agentvc-fork-attempt` | yes | Fork an attempt and switch into its view (the plugin's `/attempt`). |
| `agentvc-attempt-tree` | yes | Show the attempt tree and what is running (`/attempts`). |
| `agentvc-compare-attempts` | yes | Files and patch that differ between attempts (`/compare`). |
| `agentvc-promote-attempt` | yes | Promote a winner on the user's instruction, then rebase and open a PR (`/promote`). |
| `parallel-attempts` | optional | Run 2-4 genuinely different approaches in isolated workspaces with one acceptance check. Uses agentvc when installed, git worktrees otherwise. |
| `attempt-review` | optional | Score competing implementations (attempts, branches, worktrees, PRs) on the same checks and a rubric, recommend a winner and what to graft from the losers. |

The `agentvc-*` skills fail without the `agentvc` binary and a space
(`agentvc init`); each says so in its `compatibility` field. `parallel-attempts`
and `attempt-review` work in any git repository and are better with agentvc
(shared builds between forks, kept history, the review UI).

The Claude Code plugin in [integrations/claude-code](../integrations/claude-code)
adds hooks, the MCP server and slash commands on top; use these skills where
there is no plugin system, or alongside it.

## Install

Copy a skill directory into your agent's skills folder:

```sh
cp -r skills/parallel-attempts ~/.claude/skills/        # Claude Code, all projects
cp -r skills/parallel-attempts .claude/skills/          # Claude Code, this project
```

For other agents, use the skills folder their docs name, or a cross-agent
installer such as `npx skills add <owner>/<repo>`, which reads this directory.

## Package and validate

```sh
scripts/package-skills.py              # validate every skill against the spec (runs in CI)
scripts/package-skills.py --zip dist/skills   # one <name>.zip per skill, for marketplaces
```

The validator checks the frontmatter rules of the spec (name format and equal
to the directory, description and compatibility lengths), that `SKILL.md` is
under 500 lines, that relative links resolve, and that a package holds only
non-executable Markdown. Zips are reproducible (fixed timestamps and modes).

Bump `metadata.version` in a skill's frontmatter when its instructions change.
