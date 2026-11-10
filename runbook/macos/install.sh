#!/usr/bin/env bash
# Linker — macOS installer.
#
#   ./runbook/macos/install.sh             # build, install, load LaunchAgent
#   ./runbook/macos/install.sh --no-build  # reuse target/release/linkerd
#
# Idempotent. No sudo, no kernel extensions.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN_DIR="$HOME/.local/bin"
AGENT="$HOME/Library/LaunchAgents/io.linker.daemon.plist"
LABEL="io.linker.daemon"
DOMAIN="gui/$(id -u)"
DO_BUILD=1
for a in "$@"; do
  case "$a" in
    --no-build) DO_BUILD=0 ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

if [[ $DO_BUILD -eq 1 ]]; then
  say "Building linkerd (release)"
  if ! command -v cargo >/dev/null; then
    [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
  fi
  command -v cargo >/dev/null || { echo "!! Rust not found. Install: curl https://sh.rustup.rs -sSf | sh"; exit 1; }
  xcode-select -p >/dev/null 2>&1 || { echo "!! Install the Xcode Command Line Tools: xcode-select --install"; exit 1; }
  # cmake builds the bundled libopus (audio codec).
  if ! command -v cmake >/dev/null; then
    if command -v brew >/dev/null; then brew install cmake
    else echo "!! cmake not found. Install Homebrew (https://brew.sh) then: brew install cmake"; exit 1; fi
  fi
  (cd "$REPO" && cargo build --release -p linker-daemon)
fi

say "Stopping any running instance"
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true

say "Installing binary → $BIN_DIR/linkerd"
mkdir -p "$BIN_DIR" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
install -m 0755 "$REPO/target/release/linkerd" "$BIN_DIR/linkerd"
# A stable identifier helps macOS privacy settings recognise the binary.
# (Ad-hoc signatures change on every rebuild; see runbook: "Permissions reset after update".)
codesign --force --sign - --identifier io.linker.daemon "$BIN_DIR/linkerd"
"$BIN_DIR/linkerd" init-config >/dev/null

say "Installing LaunchAgent → $AGENT"
sed "s#__HOME__#$HOME#g" "$REPO/packaging/macos/io.linker.daemon.plist" > "$AGENT"
plutil -lint "$AGENT" >/dev/null
launchctl bootstrap "$DOMAIN" "$AGENT"
launchctl enable "$DOMAIN/$LABEL"
launchctl kickstart -k "$DOMAIN/$LABEL"

say "Done"
cat <<EOF
  Status : $BIN_DIR/linkerd status
  Logs   : tail -f ~/Library/Logs/linker.log
  Check  : $BIN_DIR/linkerd check
  Pair   : $BIN_DIR/linkerd pair      (run on both machines)

  Next: grant permissions to $BIN_DIR/linkerd in
        System Settings → Privacy & Security (see runbook/macos.md, step 4),
        then: launchctl kickstart -k $DOMAIN/$LABEL
EOF
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "  Note: add $BIN_DIR to your PATH (e.g. in ~/.zprofile)";; esac
