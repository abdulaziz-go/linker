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
| **Microphone sharing** | The MacBook's microphone shows up on Ubuntu as an input device, **"Linker Microphone"** (PipeWire virtual source). The Mac mic only runs while an Ubuntu app records from it. |
| **Speaker sharing** | Ubuntu gets an output device, **"Linker Speaker"**: whatever an app plays to it comes out of the Mac's speakers/headphones — other Ubuntu sounds stay local. |

Together: in Zoom on Ubuntu pick **Linker Camera**, **Linker Microphone** and
**Linker Speaker** and the call uses the Mac's camera, mic and speakers.
Audio is Opus low-delay with per-packet redundancy and an adaptive jitter
buffer (a port of the proven `audiobridge` design): typically 15–25 ms per
direction on a good LAN. Screen sharing, mobile apps and anything beyond the
LAN are out of scope.

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
│ features: clipboard · notifications/DND · KVM · camera · audio     │
│        └── PlatformBackend trait ──┬── linker-linux               │
│                                    └── linker-macos               │
└─────────────────────────────────────────────────────────────────────┘
```

* **One QUIC connection** carries an ordered control stream (clipboard text,
  notifications, DND, KVM events, camera + audio control), separate streams
  for large clipboard payloads, and unreliable datagrams for H.264 video and
  Opus audio (late packets are dropped, never retransmitted; lost audio is
  rebuilt from redundant copies or concealed by Opus PLC). Audio and video
  datagrams are demultiplexed into separate queues.
* **Security**: each device has a long-lived P-256 key (Keychain / Secret
  Service). Pairing shows a 6-digit code derived from both keys; afterwards
  only the pinned peer key can complete the TLS handshake.
* **Ubuntu** uses only existing kernel modules (`uinput`, `v4l2loopback`);
  audio is pure userspace PipeWire.
  **macOS** is 100 % userspace (AVFoundation + VideoToolbox, CoreAudio,
  CGEvent, NSPasteboard).

### Workspace

| Crate | Role |
|---|---|
| `crates/linker-proto` | wire types, framing, versioning, keycode tables, video fragmentation (no I/O) |
| `crates/linker-audio` | audio engine ported from audiobridge: Opus codec, redundancy packets, reorder, adaptive jitter buffer, network simulator (no I/O) |
| `crates/linker-platform` | `PlatformBackend` trait, echo guard, content hashing |
| `crates/linker-core` | discovery, transport, pairing, supervisor, features (platform-independent) |
| `crates/linker-linux` | evdev/uinput, v4l2loopback + H.264 decode, PipeWire virtual mic/speaker, clipboard, D-Bus, GSettings, logind |
| `crates/linker-macos` | AVFoundation/VideoToolbox, CoreAudio (cpal), CGEventTap/CGEvent, NSPasteboard, notifications DB, Focus, Keychain |
| `crates/linker-daemon` | the `linkerd` binary and CLI |
| `packaging/` | systemd user unit, launchd LaunchAgent |
| `runbook/` | setup guides and install/uninstall scripts |
| `docs/` | design documents |
| `prototype/` | the earlier throwaway TCP prototype, kept for reference; not built |

### Development

```bash
# Ubuntu build deps: sudo apt install cmake libpipewire-0.3-dev libspa-0.2-dev libclang-dev
# macOS build deps: brew install cmake
cargo build                      # whole workspace (on the current OS)
cargo test                       # unit + two-daemon integration tests
cargo test -p linker-linux -- --ignored --test-threads=1   # real PipeWire tests (needs a session)
./runbook/audio-e2e.sh 60        # two real daemons + PipeWire, both audio flows
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
| [Camera & microphone](docs/feature-mic-cam.md) | camera (original design) |
| [Audio](docs/feature-audio.md) | microphone + speaker: audiobridge port, transport, PipeWire/CoreAudio, testing |
