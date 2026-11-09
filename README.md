# Linker

**Linker** links one **MacBook** and one **Ubuntu desktop** over your local
Wi-Fi/LAN — peer-to-peer, encrypted, no cloud, no account. A small Rust daemon
(`linkerd`) runs on each machine, starts at boot, finds the other machine and
keeps the link alive through sleep, lid-close and Wi-Fi changes.

## Features

| | |
|---|---|
| **Universal Clipboard** | Copy on one machine, paste on the other: text, rich text, images, files. iPhone → Mac → Ubuntu works through Apple's own Universal Clipboard on the Mac. |
| **Notifications** | Mac notifications appear on Ubuntu as native banners. |
| **Do Not Disturb sync** | Toggle DND/Focus on either machine; the other follows. |
| **Software KVM** | One keyboard and mouse across both screens — push the pointer past the screen edge to move to the other machine. |
| **Camera sharing** | The MacBook's camera shows up on Ubuntu as a normal webcam, **"Linker Camera"** (`/dev/video10`), usable in browsers, Zoom, OBS. |

**Camera only — no microphone.** Linker never captures or transmits audio.
Screen sharing, mobile apps and anything beyond the LAN are out of scope.

## Getting started

Follow the **[runbook](runbook/README.md)**:

1. Ubuntu: `./runbook/linux/install.sh` — details in [runbook/ubuntu.md](runbook/ubuntu.md)
2. macOS: `./runbook/macos/install.sh` — details in [runbook/macos.md](runbook/macos.md)
3. On both: `linkerd pair`, check that both show the same 6-digit code, answer `y`.
4. `linkerd status` on either side.

## How it works

```
┌─────────────── linkerd (same core on both machines) ───────────────┐
│ discovery (mDNS _linker._udp + UDP broadcast fallback)             │
│ QUIC/TLS 1.3 link, mutual auth with pinned keys (SAS pairing)      │
│ supervisor: tie-break dialing, heartbeat, backoff, sleep/wake      │
│ features: clipboard · notifications/DND · KVM · camera             │
│        └── PlatformBackend trait ──┬── linker-linux               │
│                                    └── linker-macos               │
└─────────────────────────────────────────────────────────────────────┘
```

* **One QUIC connection** carries an ordered control stream (clipboard text,
  notifications, DND, KVM events, camera control), separate streams for large
  clipboard payloads, and unreliable datagrams for H.264 video (late frames
  are dropped, never retransmitted).
* **Security**: each device has a long-lived P-256 key (Keychain / Secret
  Service). Pairing shows a 6-digit code derived from both keys; afterwards
  only the pinned peer key can complete the TLS handshake.
* **Ubuntu** uses only existing kernel modules (`uinput`, `v4l2loopback`).
  **macOS** is 100 % userspace (AVFoundation + VideoToolbox, CGEvent,
  NSPasteboard).

### Workspace

| Crate | Role |
|---|---|
| `crates/linker-proto` | wire types, framing, versioning, keycode tables, video fragmentation (no I/O) |
| `crates/linker-platform` | `PlatformBackend` trait, echo guard, content hashing |
| `crates/linker-core` | discovery, transport, pairing, supervisor, features (platform-independent) |
| `crates/linker-linux` | evdev/uinput, v4l2loopback + H.264 decode, clipboard, D-Bus, GSettings, logind |
| `crates/linker-macos` | AVFoundation/VideoToolbox, CGEventTap/CGEvent, NSPasteboard, notifications DB, Focus, Keychain |
| `crates/linker-daemon` | the `linkerd` binary and CLI |
| `packaging/` | systemd user unit, launchd LaunchAgent |
| `runbook/` | setup guides and install/uninstall scripts |
| `docs/` | original design documents (the microphone parts there are out of scope for this version) |
| `prototype/` | the earlier throwaway TCP prototype, kept for reference; not built |

### Development

```bash
cargo build                      # whole workspace (on the current OS)
cargo test                       # unit + two-daemon integration tests
cargo clippy --all-targets
cargo check -p linker-macos --target aarch64-apple-darwin   # type-check the Mac backend from Linux
```

Measurements: [runbook/measurements.md](runbook/measurements.md).

## Design documents

| Document | Covers |
|---|---|
| [PRD](docs/PRD.md) | vision, goals, scope |
| [Core architecture](docs/core-linker.md) | daemon, transport, discovery, pairing |
| [KVM](docs/feature-kvm.md) | one keyboard/mouse |
| [Clipboard](docs/feature-cliboard.md) | universal clipboard |
| [Notifications & DND](docs/feature-notifications-dnd.md) | mirroring, focus sync |
| [Camera & microphone](docs/feature-mic-cam.md) | camera (microphone not implemented) |
