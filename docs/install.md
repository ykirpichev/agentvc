# Install agentvc

Four pieces. You need the first one; add the others for the editors you use.
Each works on its own and they all talk to the same local daemon.

## 1. The CLI and the review UI (agentvc + agentvcd)

macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/ykirpichev/agentvc/main/scripts/get.sh | sh
```

That downloads the latest release for your machine into `~/.local/bin`
(set `AGENTVC_INSTALL_DIR` to change it) and says if that folder is not on
your `PATH`. Run it again to upgrade. Add `-s -- --vscode` to also install
the editor extension into every `code` and `cursor` it finds.

By hand, or on Windows: download `agentvc-<version>-<target>.tar.gz` (`.zip`
on Windows) from the [latest release](https://github.com/ykirpichev/agentvc/releases/latest),
unpack it, and put `agentvc` and `agentvcd` on your `PATH`. On macOS, if the
binaries were downloaded with a browser, clear the quarantine flag once:
`xattr -d com.apple.quarantine agentvc agentvcd`.

Then, in each repository:

```sh
cd ~/src/my-repo
agentvcd                    # review UI on http://localhost:7420 (leave it running)
agentvc stack create my-fix -am "Fix it"   # or start work from the UI
```

Check it with `agentvc --version`.

## 2. VS Code

Download `agentvc.vsix` from the [latest release](https://github.com/ykirpichev/agentvc/releases/latest), then:

```sh
code --install-extension agentvc.vsix
```

(or *Extensions → … → Install from VSIX…*). The agentvc icon in the activity
bar opens the Branches panel; it starts `agentvcd` for you when it is not
running.

## 3. Cursor

Search **agentvc** in the Extensions view (it comes from Open VSX), or install
the same file:

```sh
cursor --install-extension agentvc.vsix
```

## 4. Claude Code

The plugin adds the agentvc MCP server, hooks, slash commands and a skill; the
panel plugin shows your branch tree beside the chat. From a shell:

```sh
claude plugin marketplace add ykirpichev/agentvc
claude plugin install agentvc@agentvc          # MCP server, hooks, commands
claude plugin install agentvc-panel@agentvc    # the branch tree pane
```

or at the Claude Code prompt: `/plugin marketplace add ykirpichev/agentvc`,
then `/plugin install agentvc@agentvc`. Both need `agentvc` on your `PATH` and
the panel needs `agentvcd` running. To upgrade:
`claude plugin marketplace update agentvc`.

The release also has the two plugins as zips
(`agentvc-claude-code-plugin.zip`, `agentvc-claude-panel-plugin.zip`) for
offline installs: unzip and run `claude --plugin-dir <folder>`.

Just the MCP server, without the plugin: `claude mcp add agentvc -- agentvc mcp`.

## Coming from Graphite

[docs/switch-from-graphite.md](https://github.com/ykirpichev/agentvc/blob/main/docs/switch-from-graphite.md)
has the day-one steps and the `gt` to `agentvc stack` table.
