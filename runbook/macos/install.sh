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

# --- Code signing -----------------------------------------------------------
# macOS privacy (TCC) remembers a grant by the binary's *designated requirement*.
# For an AD-HOC signature ("--sign -") that requirement is the cdhash, which
# changes on every rebuild — so Accessibility / Full Disk Access / Microphone
# grants are lost each upgrade. Signing with a STABLE identity makes the
# requirement depend on the certificate instead, so grants survive rebuilds.
#
# Identity selection (first that works):
#   1. $LINKER_CODESIGN_IDENTITY  (e.g. a "Developer ID Application: …" or any
#      self-made identity you already trust)
#   2. an existing self-signed "Linker Code Signing" identity in the login keychain
#   3. auto-create that self-signed identity (needs a modern openssl, e.g.
#      `brew install openssl@3`)
#   4. fall back to ad-hoc (grants will reset on each rebuild — see runbook)
SIGN_ID="io.linker.daemon"
STABLE_NAME="Linker Code Signing"

pick_identity() {
  if [[ -n "${LINKER_CODESIGN_IDENTITY:-}" ]]; then
    echo "$LINKER_CODESIGN_IDENTITY"; return 0
  fi
  if security find-identity -v -p codesigning 2>/dev/null | grep -qF "$STABLE_NAME"; then
    echo "$STABLE_NAME"; return 0
  fi
  return 1
}

create_stable_identity() {
  # Prefer a real OpenSSL (LibreSSL that ships with macOS lacks -addext).
  local OSSL=""
  for cand in "$(brew --prefix openssl@3 2>/dev/null)/bin/openssl" \
              "$(brew --prefix openssl 2>/dev/null)/bin/openssl" openssl; do
    if "$cand" version 2>/dev/null | grep -qi "openssl"; then OSSL="$cand"; break; fi
  done
  [[ -n "$OSSL" ]] || { echo "   (no modern openssl; skipping stable identity)"; return 1; }

  local tmp; tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  "$OSSL" req -x509 -newkey rsa:2048 -nodes -days 3650 \
      -keyout "$tmp/key.pem" -out "$tmp/cert.pem" \
      -subj "/CN=$STABLE_NAME" \
      -addext "basicConstraints=critical,CA:false" \
      -addext "keyUsage=critical,digitalSignature" \
      -addext "extendedKeyUsage=critical,codeSigning" >/dev/null 2>&1 || return 1
  "$OSSL" pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
      -out "$tmp/id.p12" -name "$STABLE_NAME" -passout pass:linker >/dev/null 2>&1 || return 1
  # Import the identity and allow codesign to use it without prompting.
  security import "$tmp/id.p12" -P linker \
      -k "$HOME/Library/Keychains/login.keychain-db" \
      -T /usr/bin/codesign >/dev/null 2>&1 || return 1
  security find-identity -v -p codesigning 2>/dev/null | grep -qF "$STABLE_NAME"
}

IDENTITY="$(pick_identity || true)"
if [[ -z "$IDENTITY" ]]; then
  say "Creating a stable self-signed signing identity (\"$STABLE_NAME\")"
  if create_stable_identity; then IDENTITY="$STABLE_NAME"; fi
fi

if [[ -n "$IDENTITY" ]]; then
  codesign --force --options runtime --sign "$IDENTITY" --identifier "$SIGN_ID" "$BIN_DIR/linkerd"
  echo "   Signed with stable identity: $IDENTITY"
  echo "   → Accessibility / Full Disk Access / Microphone grants now survive rebuilds."
else
  codesign --force --sign - --identifier "$SIGN_ID" "$BIN_DIR/linkerd"
  cat <<'WARN'
   !! Signed AD-HOC (no stable identity available).
      macOS will forget linkerd's permissions on every rebuild. After each
      upgrade you must re-grant them:
        System Settings → Privacy & Security →
          Accessibility, Full Disk Access, Input Monitoring, Microphone
        Remove the old "linkerd" entry (select it, press –) and add
        ~/.local/bin/linkerd again, then:
          launchctl kickstart -k gui/$(id -u)/io.linker.daemon
      To avoid this, install a modern openssl (brew install openssl@3) and
      rerun this script, or set LINKER_CODESIGN_IDENTITY to a signing identity.
WARN
fi
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
