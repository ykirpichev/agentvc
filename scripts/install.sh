#!/usr/bin/env bash
# Install agentvc and agentvcd (with the review UI built in) for use in any
# repository, e.g. your own projects. Run from an agentvc checkout; safe to
# run again to upgrade. See docs/switch-from-graphite.md.
#
#   scripts/install.sh            # binaries into ~/.cargo/bin
#   scripts/install.sh --vscode   # also build the VS Code / Cursor extension
#                                 # (integrations/vscode/agentvc.vsix) and
#                                 # install it where `code` / `cursor` exist
set -euo pipefail
cd "$(dirname "$0")/.."

vscode=0
for a in "$@"; do
  case "$a" in
    --vscode) vscode=1 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

for tool in cargo npm git; do
  command -v "$tool" >/dev/null || { echo "install.sh needs $tool on PATH" >&2; exit 1; }
done

# The UI first: release builds of agentvcd embed ui/dist at compile time.
echo "==> building the review UI"
(cd ui && npm ci --silent && npm run build --silent)

echo "==> installing agentvc and agentvcd"
cargo install --locked --quiet --force --path crates/agentvc-cli
cargo install --locked --quiet --force --path crates/agentvcd

if [ "$vscode" = 1 ]; then
  echo "==> building the VS Code / Cursor extension"
  (cd integrations/vscode && npm ci --silent && npm run package --silent)
  for editor in code cursor; do
    if command -v "$editor" >/dev/null; then
      "$editor" --install-extension integrations/vscode/agentvc.vsix --force >/dev/null &&
        echo "    installed in $editor"
    fi
  done
fi

bin=$(dirname "$(command -v agentvc 2>/dev/null || echo "$HOME/.cargo/bin/agentvc")")
cat <<MSG

Installed: $("$bin/agentvc" --version 2>/dev/null || echo agentvc)
Make sure $bin is on your PATH. In each project:

  cd ~/src/my-project
  agentvc stack adopt <top branch>   # or: agentvc stack create -am "First change"
  agentvcd                            # review UI on http://localhost:7420

The full cutover steps are in docs/switch-from-graphite.md.
MSG
