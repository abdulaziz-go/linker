# Linker — Documentation Index

**Linker** (working codename) is a personal continuity system that links a
MacBook and an Ubuntu desktop over the local Wi-Fi network, the way macOS and
iPhone work together: shared clipboard, mirrored notifications, synced
Do-Not-Disturb, one keyboard/mouse across both screens, and the Mac's
camera/microphone usable on Ubuntu as if they were local devices.

No cloud. No Apple account dependency. Both machines run the same core package
and talk directly to each other on the LAN.

## How to read these documents

Read them in order. The PRD tells you *what* we are building and *why*. The
core architecture doc explains the shared foundation every feature stands on.
Then each feature has its own design doc with a high-level section (how it
works, in plain language) and a low-level section (exact protocols, data
formats, libraries, and kernel/privileged pieces).

| # | Document | What it covers |
|---|----------|----------------|
| 00 | [Product Requirements (PRD)](docs/PRD.md) | Vision, goals, features, scope, roadmap |
| 01 | [Core Architecture](docs/core-linker.md) | The common package, daemon, transport, discovery, pairing, security, kernel strategy |
| 02 | [Software KVM](docs/feature-kvm.md) | One keyboard/mouse across both screens |
| 03 | [Universal Clipboard](docs/feature-cliboard.md) | Copy/paste of all content types, including the iPhone → Mac → Ubuntu chain |
| 04 | [Notifications & Do Not Disturb](docs/feature-notifications-dnd.md) | Mirrored notifications, synced focus/DND state |
| 05 | [Camera & Microphone Sharing](docs/feature-mic-cam.md) | Mac camera/mic as real devices on Ubuntu |

## Status

These are design documents. No production code is written yet. A throwaway
connection prototype exists only to validate discovery and the link layer; it
is not part of this design and will be rebuilt from the architecture below.

## Key decisions at a glance

- **Language:** Rust on both platforms. One core crate, platform backends behind traits.
- **Transport:** QUIC (encrypted, multiplexed) for the data plane; mDNS for discovery.
- **Topology:** Direct peer-to-peer on the LAN. No server, no relay.
- **macOS kernel:** Untouched. Everything on the Mac is userspace. (Mac is the capture *source*, so it needs no virtual devices.)
- **Ubuntu kernel:** Uses existing modules only — `uinput` (input), `v4l2loopback` (virtual camera), PipeWire (virtual mic). A custom kernel module is **not required** for v1; it is noted as an optional future optimization.