#!/usr/bin/env bash
# Linker — Ubuntu uninstaller.
#   ./runbook/linux/uninstall.sh            # remove service + binary (keeps config/pairing)
#   ./runbook/linux/uninstall.sh --purge    # also delete ~/.config/linker (identity + pairing)
#   ./runbook/linux/uninstall.sh --system   # also remove module/udev config (sudo)
set -euo pipefail
PURGE=0; SYSTEM=0
for a in "$@"; do
  case "$a" in --purge) PURGE=1 ;; --system) SYSTEM=1 ;; *) echo "unknown option $a" >&2; exit 2 ;; esac
done

systemctl --user disable --now linker.service 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/linker.service"
systemctl --user daemon-reload || true
rm -f "$HOME/.local/bin/linkerd"
rm -rf "$HOME/.local/share/doc/linker"
echo "Removed service and binary."

if [[ $PURGE -eq 1 ]]; then
  rm -rf "$HOME/.config/linker"
  command -v secret-tool >/dev/null && secret-tool clear service io.linker key identity-key 2>/dev/null || true
  echo "Removed configuration, identity and pairing."
fi

if [[ $SYSTEM -eq 1 ]]; then
  sudo rm -f /etc/modules-load.d/linker.conf /etc/modprobe.d/linker-v4l2loopback.conf \
             /etc/udev/rules.d/70-linker-uinput.rules
  sudo udevadm control --reload-rules
  sudo modprobe -r v4l2loopback 2>/dev/null || true
  echo "Removed kernel module and udev configuration (packages left installed)."
fi
echo "Note: 'loginctl enable-linger' is left on (other services may rely on it)."
