# Linker Runbook

Set up Linker on both machines by following these in order:

1. **Ubuntu** — [ubuntu.md](ubuntu.md) (`./runbook/linux/install.sh`)
2. **macOS** — [macos.md](macos.md) (`./runbook/macos/install.sh`)
3. **Pair** — run `linkerd pair` on both and compare the 6-digit code.
4. **Verify** — `linkerd check` and `linkerd status` on both.

Scripts:

| Script | Purpose |
|---|---|
| `linux/install.sh` | packages (incl. PipeWire/Opus build deps), `uinput` + `v4l2loopback` ("Linker Camera"), udev/groups, firewall, build, systemd user service, linger |
| `linux/uninstall.sh` | remove service/binary (`--purge` config+pairing, `--system` module/udev config) |
| `macos/install.sh` | cmake (Homebrew), build, install, sign, LaunchAgent |
| `audio-e2e.sh` | developer tool: two local daemons + PipeWire, both audio flows end to end (`steady`, `sleepwake`, `restart`, `impair`) |
| `macos/uninstall.sh` | remove agent/binary (`--purge` config, Keychain identity, pairing) |

Measurements and how to reproduce them: [measurements.md](measurements.md).

## Quick reference

```text
linkerd status        link state, peer, per-feature health, camera + audio stats
linkerd check         probe OS capabilities and permissions (incl. audio devices, latency, loss)
linkerd pair          pair with the other machine (both sides)
linkerd unpair        forget the paired machine
linkerd id            show device id and file locations
linkerd -v run        run in the foreground with debug logs (stop the service first)
```

| | Ubuntu | macOS |
|---|---|---|
| Binary | `~/.local/bin/linkerd` | `~/.local/bin/linkerd` |
| Config | `~/.config/linker/config.toml` | `~/Library/Application Support/linker/config.toml` |
| Pairing | `~/.config/linker/paired.toml` | `~/Library/Application Support/linker/paired.toml` |
| Identity key | Secret Service (else `identity.key`, 0600) | login Keychain (`io.linker`) |
| Service | `systemctl --user … linker` | `launchctl … gui/$UID/io.linker.daemon` |
| Logs | `journalctl --user -u linker -f` | `~/Library/Logs/linker.log` |

Network: UDP **47800** (QUIC link), UDP **47801** (broadcast discovery
fallback), UDP **5353** (mDNS). Everything stays on the LAN.
