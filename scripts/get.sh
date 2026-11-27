#!/bin/sh
# Download and install the latest agentvc release (agentvc + agentvcd with the
# review UI built in). Safe to run again to upgrade.
#
#   curl -fsSL https://raw.githubusercontent.com/ykirpichev/agentvc/main/scripts/get.sh | sh
#   ... | sh -s -- --vscode          # also install the VS Code / Cursor extension
#   ... | sh -s -- --version 0.2.0   # a given release
#
# AGENTVC_INSTALL_DIR picks the folder (default ~/.local/bin). To build from a
# checkout instead, use scripts/install.sh.
set -eu

repo=ykirpichev/agentvc
dir=${AGENTVC_INSTALL_DIR:-$HOME/.local/bin}
version=""
vscode=0
while [ $# -gt 0 ]; do
  case "$1" in
    --vscode) vscode=1 ;;
    --version) version=${2:?--version needs a value}; shift ;;
    -h|--help) sed -n '2,11p' "$0" 2>/dev/null || true; exit 0 ;;
    *) echo "get.sh: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) echo "get.sh: $(uname -s) is not supported here; download a release by hand: https://github.com/$repo/releases" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64|amd64) arch=x86_64 ;;
  *) echo "get.sh: unsupported CPU $(uname -m)" >&2; exit 1 ;;
esac
target="$arch-$os"

if [ -z "$version" ]; then
  # The latest release's tag, from the redirect of /releases/latest.
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")
  version=${url##*/}
fi
version=${version#v}
base="https://github.com/$repo/releases/download/v$version"
name="agentvc-$version-$target"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "==> downloading agentvc $version for $target"
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz"
if curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" 2>/dev/null; then
  want=$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
  if command -v sha256sum >/dev/null; then got=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
  else got=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1); fi
  [ -z "$want" ] || [ "$want" = "$got" ] || { echo "get.sh: checksum mismatch for $name.tar.gz" >&2; exit 1; }
fi
tar xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
for b in agentvc agentvcd; do
  # Replace, don't overwrite in place: a running agentvcd keeps its old file.
  cp "$tmp/$name/$b" "$dir/.$b.new" && chmod 755 "$dir/.$b.new" && mv -f "$dir/.$b.new" "$dir/$b"
done
echo "    installed agentvc and agentvcd in $dir"

if [ "$vscode" = 1 ]; then
  curl -fsSL "$base/agentvc.vsix" -o "$tmp/agentvc.vsix"
  for editor in code cursor; do
    if command -v "$editor" >/dev/null 2>&1; then
      "$editor" --install-extension "$tmp/agentvc.vsix" --force >/dev/null && echo "    installed the extension in $editor"
    fi
  done
fi

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "
$dir is not on your PATH. Add this to your shell profile:
  export PATH=\"$dir:\$PATH\"" ;;
esac
echo "
$("$dir/agentvc" --version 2>/dev/null || echo agentvc) is ready. Restart agentvcd if it is running.
In a repository: agentvcd   (review UI on http://localhost:7420)
Editors and Claude Code: https://github.com/$repo/blob/main/docs/install.md"
