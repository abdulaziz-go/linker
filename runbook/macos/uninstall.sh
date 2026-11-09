#!/usr/bin/env bash
# Linker — macOS uninstaller.
#   ./runbook/macos/uninstall.sh          # remove LaunchAgent + binary (keeps config/pairing)
#   ./runbook/macos/uninstall.sh --purge  # also delete config, identity (Keychain) and pairing
set -euo pipefail
PURGE=0
[[ "${1:-}" == "--purge" ]] && PURGE=1

launchctl bootout "gui/$(id -u)/io.linker.daemon" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/io.linker.daemon.plist"
rm -f "$HOME/.local/bin/linkerd"
echo "Removed LaunchAgent and binary."

if [[ $PURGE -eq 1 ]]; then
  rm -rf "$HOME/Library/Application Support/linker"
  security delete-generic-password -s io.linker -a identity-key >/dev/null 2>&1 || true
  echo "Removed configuration, Keychain identity and pairing."
fi
echo "You can also remove linkerd from System Settings → Privacy & Security lists."
