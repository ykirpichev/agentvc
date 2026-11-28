# agentvc for Claude Code

One install turns agentvc on in Claude Code:

- **Hooks** snapshot the attempt you are working in after every edit (`Edit`,
  `Write`, `MultiEdit`, `NotebookEdit`, `Bash`) and at the end of every turn.
  An edit is recorded in the attempt whose view holds the edited file, even if
  the shell is somewhere else. Outside an agentvc space the hooks do nothing.
- **MCP server** (`agentvc mcp`): the agent works on the workspace's stack
  (`stack_log`, `stack_create`, `stack_modify`, `stack_submit`, `stack_sync`,
  `stack_restack`) and answers review comments (`review_comments`,
  `reply_comment`, `resolve_comment`). Tools show up as
  `mcp__plugin_agentvc_agentvc__<tool>`. The attempt-tree tools (`fork`,
  `snapshot`, `promote`, ...) need `agentvc mcp --attempts`.
- **Slash commands**, each printing a link to the review UI:
  - `/attempt [--from aN] [--task tN] [what you will try]`: fork a new attempt
    (from the current one by default, on its parent's task) and switch the
    session into its view.
  - `/attempts [tN]`: the attempt tree.
  - `/compare aX aY [aZ...]`: files that differ, and the side-by-side compare.
  - `/promote aN`: pick the winner (only you can run this one, not the model).

  If another plugin uses the same name, use the namespaced form, for example
  `/agentvc:attempt`.
- **Skill** `stack-workflow`: teaches the agent to commit one branch per change
  with `stack_create`, put fixes into their branch with `stack_modify`, submit
  PRs only when asked, and answer review comments in their threads.

## Install

You need the `agentvc` binary on your `PATH` (or set `AGENTVC_BIN`), and a
space in the repository (`agentvc init`).

In Claude Code:

```
/plugin marketplace add ykirpichev/agentvc
/plugin install agentvc@agentvc
```

or from a shell:

```sh
claude plugin marketplace add ykirpichev/agentvc
claude plugin install agentvc@agentvc
```

From a local checkout, use the path instead: `claude plugin marketplace add ./`
in the agentvc repository, or try it for one session with
`claude --plugin-dir integrations/claude-code`.

No settings file is edited. Run `/mcp` to see the `agentvc` server and `/hooks`
to see the hooks.

Environment variables (optional):

| Variable | Default | Use |
| --- | --- | --- |
| `AGENTVC_BIN` | `agentvc` on `PATH` | The agentvc binary to run |
| `AGENTVC_UI_URL` | `http://localhost:7420` | Base of the review UI links |

Start the review UI with `agentvcd`. The slash commands say so when it is not
running.

## Uninstall

```sh
claude plugin uninstall agentvc@agentvc
```

The hooks, MCP server, commands and skill go with the plugin: nothing was
written to your settings, so nothing is left behind.

### If you used `agentvc hooks` before

`agentvc hooks` writes the same hooks into `.claude/settings.local.json` (or
`~/.claude/settings.json` with `--user`). While those are installed, the
plugin's hooks stand down so each edit is snapshotted once. To move to the
plugin, remove them with `agentvc hooks --uninstall` (and `--user`).

## Claude Desktop

Desktop has no plugins, but it can use the MCP server, so a Desktop chat can
inspect and drive attempts. Desktop starts servers without a working
directory, so give absolute paths for both the binary and the space. Edit
`claude_desktop_config.json` (Settings > Developer > Edit Config; on macOS it is
`~/Library/Application Support/Claude/claude_desktop_config.json`, on Windows
`%APPDATA%\Claude\claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "agentvc": {
      "command": "/absolute/path/to/agentvc",
      "args": ["mcp", "--space", "/absolute/path/to/your/repo"]
    }
  }
}
```

Restart Desktop. The agentvc tools appear under the tools menu of a new chat;
ask, for example, "show me the agentvc attempt tree" or "compare a3 and a5".
One entry serves one space; add another entry (with another name) per
repository.

Check the same command outside Desktop with the MCP inspector:

```sh
npx @modelcontextprotocol/inspector --cli /absolute/path/to/agentvc mcp \
  --space /absolute/path/to/your/repo --method tools/list
```

## Layout

```
.claude-plugin/plugin.json      manifest
hooks/hooks.json                PostToolUse, Stop, SubagentStop
.mcp.json                       the agentvc MCP server
commands/                       /attempt, /attempts, /compare, /promote
skills/stack-workflow/          the stack workflow skill
bin/agentvc-plugin              entry point for all of the above
tests/plugin.sh                 smoke test (runs in CI)
```

The marketplace that lists this plugin is `.claude-plugin/marketplace.json` at
the repository root.
