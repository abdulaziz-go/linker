#!/usr/bin/env bash
# Linker — Ubuntu installer.
#
#   ./runbook/linux/install.sh              # everything (asks for sudo once for system setup)
#   ./runbook/linux/install.sh --no-system  # only build + user service (no sudo)
#   ./runbook/linux/install.sh --no-build   # reuse target/release/linkerd
#
# Idempotent: safe to run again after pulling new code.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN_DIR="$HOME/.local/bin"
UNIT_DIR="$HOME/.config/systemd/user"
DOC_DIR="$HOME/.local/share/doc/linker"
DO_SYSTEM=1
DO_BUILD=1
for a in "$@"; do
  case "$a" in
    --no-system) DO_SYSTEM=0 ;;
    --no-build) DO_BUILD=0 ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

if [[ $DO_SYSTEM -eq 1 ]]; then
  say "System setup (sudo): packages, kernel modules, device permissions, firewall"
  sudo -v

  say "Installing packages: build tools, v4l2loopback, v4l-utils, libsecret-tools"
  sudo apt-get update -qq
  sudo apt-get install -y build-essential pkg-config curl \
      v4l2loopback-dkms v4l-utils libsecret-tools "linux-headers-$(uname -r)" || {
        echo "!! package install failed — see runbook/ubuntu.md (Troubleshooting)"; exit 1; }

  say "Loading uinput + v4l2loopback at boot"
  echo -e "uinput\nv4l2loopback" | sudo tee /etc/modules-load.d/linker.conf >/dev/null
  echo 'options v4l2loopback devices=1 video_nr=10 card_label="Linker Camera" exclusive_caps=1' \
    | sudo tee /etc/modprobe.d/linker-v4l2loopback.conf >/dev/null
  sudo modprobe uinput
  if ! grep -qs "Linker Camera" /sys/class/video4linux/*/name; then
    # Reload with our options (fails harmlessly if another app holds a loopback device).
    sudo modprobe -r v4l2loopback 2>/dev/null || true
    sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="Linker Camera" exclusive_caps=1 || \
      echo "!! could not load v4l2loopback now (Secure Boot? see runbook). It will load at next boot."
  fi

  say "Device permissions: /dev/uinput → group input; user → groups input, video"
  echo 'KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' \
    | sudo tee /etc/udev/rules.d/70-linker-uinput.rules >/dev/null
  sudo udevadm control --reload-rules
  sudo udevadm trigger --name-match=uinput || true
  sudo usermod -aG input,video "$USER"

  if command -v ufw >/dev/null && sudo ufw status | grep -q "Status: active"; then
    say "Firewall (ufw): allowing Linker on the LAN"
    sudo ufw allow 47800/udp comment 'Linker QUIC'
    sudo ufw allow 47801/udp comment 'Linker discovery broadcast'
    sudo ufw allow 5353/udp comment 'mDNS'
  fi
fi

if [[ $DO_BUILD -eq 1 ]]; then
  say "Building linkerd (release)"
  if ! command -v cargo >/dev/null; then
    [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
  fi
  command -v cargo >/dev/null || { echo "!! Rust not found. Install: curl https://sh.rustup.rs -sSf | sh"; exit 1; }
  (cd "$REPO" && cargo build --release -p linker-daemon)
fi

say "Installing binary → $BIN_DIR/linkerd"
mkdir -p "$BIN_DIR" "$UNIT_DIR" "$DOC_DIR"
# Stop first so the binary isn't busy.
systemctl --user stop linker.service 2>/dev/null || true
install -m 0755 "$REPO/target/release/linkerd" "$BIN_DIR/linkerd"
cp "$REPO/runbook/ubuntu.md" "$DOC_DIR/" 2>/dev/null || true
"$BIN_DIR/linkerd" init-config >/dev/null

say "Installing systemd user service"
install -m 0644 "$REPO/packaging/linux/linker.service" "$UNIT_DIR/linker.service"
systemctl --user daemon-reload
systemctl --user enable --now linker.service
loginctl enable-linger "$USER"

say "Done"
cat <<EOF
  Status : $BIN_DIR/linkerd status
  Logs   : journalctl --user -u linker -f
  Check  : $BIN_DIR/linkerd check
  Pair   : $BIN_DIR/linkerd pair      (run on both machines)

EOF
if [[ $DO_SYSTEM -eq 1 ]] && ! id -nG | grep -qw input; then
  echo "  !! You were added to the 'input' and 'video' groups: LOG OUT AND BACK IN"
  echo "     (or reboot) before KVM and the camera will work."
fi
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "  Note: add $BIN_DIR to your PATH (e.g. in ~/.profile)";; esac
