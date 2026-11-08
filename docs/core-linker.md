# Linker — Core Architecture (Common Package & Daemon)

**Status:** Draft v1 · **Scope:** Feature F0 (the shared foundation every other
feature depends on)

---

## 1. What this document covers

This is the "common fast package" that runs identically on the MacBook and the
Ubuntu desktop. It provides: device discovery, a secure encrypted transport,
pairing/trust, the message format all features use, the always-on daemon
lifecycle (start at boot, auto-reconnect), and the rules for how platform-
specific code plugs in. Individual features (clipboard, KVM, camera, …) build on
top of this and are documented separately.

Two design principles drive everything here:

1. **One core, thin edges.** All logic that can be shared is shared in one Rust
   crate. Only the unavoidable per-OS pieces live behind a trait.
2. **Reliability is a feature, not an afterthought.** The link is expected to
   break (sleep, roaming, lock) and must heal itself silently.

---

## 2. High-level design

### 2.1 Component map

```
┌──────────────────────────── linker daemon (one per machine) ───────────────────────────┐
│                                                                                         │
│   ┌───────────────┐   ┌──────────────────┐   ┌───────────────────────────────────┐     │
│   │  Discovery     │   │   Transport       │   │        Feature modules            │     │
│   │  (mDNS)        │──▶│  (QUIC + TLS)      │◀─▶│ clipboard · notif/DND · kvm ·     │     │
│   │  find peer     │   │  control + media   │   │ camera · mic                      │     │
│   └───────────────┘   └──────────────────┘   └───────────────────────────────────┘     │
│           │                    │                                 │                        │
│           ▼                    ▼                                 ▼                        │
│   ┌───────────────┐   ┌──────────────────┐   ┌───────────────────────────────────┐     │
│   │ Supervisor     │   │  Pairing / trust  │   │      Platform backend (trait)     │     │
│   │ (reconnect,    │   │  (device keys,    │   │  macOS impl   |   Linux impl       │     │
│   │  not-found)    │   │   cert pinning)   │   │  (AVF, CGEvent│  (evdev, uinput,   │     │
│   └───────────────┘   └──────────────────┘   │   NSPasteboard│   v4l2loopback,     │     │
│                                                │   …)          │   PipeWire, …)      │     │
│                                                └───────────────────────────────────┘     │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 The two planes

Linker separates traffic into two "planes" because they have opposite needs:

- **Control plane** — reliable, ordered, low-rate. Carries commands and small
  payloads: pairing, clipboard text, notifications, DND toggles, KVM
  key/mouse events, control messages for the media features. Loss is not
  acceptable; a few milliseconds of latency is fine.
- **Data plane** — high-rate, latency-sensitive, loss-tolerant. Carries
  continuous audio and video for camera/mic sharing. A dropped frame is better
  than a late frame.

QUIC gives us both over one encrypted connection: reliable streams for control,
and (for media) either dedicated streams or an unreliable datagram extension so
a late audio/video packet is skipped rather than retransmitted.

---

## 3. The common package (crate layout)

A Cargo workspace. The core is OS-independent; platform crates implement the
backend trait.

```
linker/
├── crates/
│   ├── linker-proto/      # message types, serialization, versioning (no I/O)
│   ├── linker-core/       # discovery, transport, pairing, supervisor, feature
│   │                     #   dispatch — all async, all platform-independent
│   ├── linker-platform/   # the PlatformBackend trait + shared helpers
│   ├── linker-macos/      # macOS backend impl (AVFoundation, CGEvent, …)
│   ├── linker-linux/      # Linux backend impl (evdev, uinput, v4l2loopback, …)
│   └── linker-daemon/     # the `linkerd` binary: wires core + the right backend
└── docs/
```

`linker-core` never calls an OS API directly. It calls methods on a
`PlatformBackend` object. At startup, `linker-daemon` constructs the correct
backend (`linker-macos` or `linker-linux`) via `#[cfg(target_os = …)]` and hands
it to the core. This is what the PRD means by "one core, thin edges."

```rust
// linker-platform (sketch — not final)
#[async_trait]
pub trait PlatformBackend: Send + Sync {
    // Clipboard
    async fn clipboard_read(&self) -> anyhow::Result<ClipboardContent>;
    async fn clipboard_write(&self, c: &ClipboardContent) -> anyhow::Result<()>;
    fn clipboard_watch(&self) -> ClipboardEvents; // stream of changes

    // Notifications & DND
    async fn show_notification(&self, n: &Notification) -> anyhow::Result<()>;
    async fn set_dnd(&self, on: bool) -> anyhow::Result<()>;
    fn dnd_watch(&self) -> DndEvents;

    // Input (KVM)
    fn input_capture(&self) -> InputEvents;      // grab local keyboard/mouse
    async fn input_inject(&self, e: &InputEvent) -> anyhow::Result<()>;

    // Media (camera/mic) — present only where applicable
    fn camera_source(&self) -> Option<Box<dyn VideoSource>>;   // macOS: real camera
    fn camera_sink(&self) -> Option<Box<dyn VideoSink>>;       // Linux: v4l2loopback
    fn mic_source(&self) -> Option<Box<dyn AudioSource>>;      // macOS: real mic
    fn mic_sink(&self) -> Option<Box<dyn AudioSink>>;          // Linux: virtual mic
}
```

Capabilities that a platform can't do return `None`/unsupported, and the feature
module simply doesn't activate there. For example, the MacBook provides a
`camera_source` but no `camera_sink`; Ubuntu provides a `camera_sink` but
typically no `camera_source`.

---

## 4. Low-level design

### 4.1 Discovery — mDNS / DNS-SD

Each daemon advertises a service of type `_linker._udp.local` (QUIC runs over
UDP) with a TXT record carrying: `device_id`, `device_name`, `device_type`,
`proto_version`, and the QUIC port. Each daemon also browses for the same
service type. When it sees the *other* paired device, it hands the discovered
address+port to the supervisor.

- **Why mDNS over raw UDP broadcast:** mDNS is the standard both OSes already
  run (Bonjour on macOS, Avahi on Ubuntu), works across typical home APs, and
  gives name + metadata resolution for free. Raw broadcast remains a fallback.
- **Rust libraries:** `mdns-sd` (pure-Rust, no Avahi dependency) is the primary
  choice; falling back to the system responder is possible but adds coupling.
- **Self-filtering:** ignore advertisements whose `device_id` equals our own.
- **Default port:** QUIC/UDP **5400** (configurable). Discovery via standard
  mDNS port 5353.

### 4.2 Transport — QUIC over TLS 1.3

The transport is **QUIC** via the `quinn` crate (which uses `rustls` for TLS
1.3). One QUIC connection between the two daemons carries everything:

- **Control streams:** bidirectional reliable QUIC streams, one logical stream
  per concern (or a single multiplexed control stream with typed frames).
- **Media:** separate unidirectional streams per track (video, audio), or QUIC
  **datagrams** for frames that should be dropped rather than retransmitted when
  late. (Choice per media type; see the camera/mic doc.)

Why QUIC rather than TCP+TLS (what the throwaway prototype used):

- Built-in TLS 1.3 encryption and modern handshake.
- **Stream multiplexing without head-of-line blocking:** a stalled video frame
  does not hold up clipboard text, and vice versa.
- **Connection migration:** a QUIC connection is identified by a connection ID,
  not the IP/port 4-tuple, so a Wi-Fi roam or address change can be survived
  without a full reconnect — directly useful for the "survives sleep/roaming"
  reliability goal.
- Unreliable datagrams available for real-time media.

### 4.3 Pairing and trust

First-run pairing establishes mutual trust, then every later connection is
authenticated automatically.

1. Each device generates a long-lived key pair on first launch and derives a
   self-signed certificate; the `device_id` is the fingerprint of its public
   key. The private key is stored in the OS keystore where possible (macOS
   Keychain; on Linux the Secret Service / libsecret, falling back to a
   permission-restricted file in the config dir).
2. **Pairing:** when two unpaired devices meet, they show a short numeric code
   derived from both certificates (an SAS — short authentication string). The
   user confirms the same code on both sides. On confirmation, each device
   stores the other's certificate fingerprint in its trust store.
3. **Every later connection** uses mutual TLS: each side presents its
   certificate, and the handshake only succeeds if the presented certificate
   matches the pinned fingerprint from pairing. This prevents a stranger on the
   same Wi-Fi from connecting or impersonating the peer.

### 4.4 Message format

All control-plane messages are typed. The wire encoding is a compact binary
format (`bincode` or `postcard` over `serde`) rather than JSON, for speed and
size on the hot path (KVM, media control). A message is length-prefixed on a
QUIC stream:

```
┌────────────┬───────────────────────────────┐
│ len: u32   │ body: serialized Message        │
└────────────┴───────────────────────────────┘
```

```rust
// linker-proto (sketch)
pub enum Message {
    // link
    Hello(Identity),
    Ping(u64), Pong(u64),
    // features (payloads defined in each feature's doc)
    Clipboard(ClipboardMsg),
    Notification(NotificationMsg),
    Dnd(DndMsg),
    Input(InputMsg),
    MediaControl(MediaControlMsg),
    // extensibility
    Unknown,                    // forward-compat: ignore unrecognized variants
}
```

**Versioning:** every message set carries `proto_version`. Unknown variants
decode to `Unknown` and are ignored, so a newer peer can add messages without
breaking an older one. Breaking changes bump the major version and are refused
at `Hello`.

### 4.5 Daemon lifecycle and the supervisor

`linkerd` runs as a background service and owns a small state machine per peer:

```
        ┌──────────┐  peer seen (mDNS)   ┌───────────┐  mTLS ok   ┌───────────┐
        │ SEARCHING │ ──────────────────▶│ CONNECTING │──────────▶│ CONNECTED  │
        └──────────┘                     └───────────┘            └───────────┘
             ▲                                 │  fail                  │ heartbeat
             │        no peer > T_notfound     ▼                        │ timeout / EOF
             │  (fire ONE "not found" notice)  └────────────────────────┘
             └─────────────────────────────────────────────────────────┘
```

- **SEARCHING:** browsing mDNS. If no peer connection for `T_notfound`
  (default 60s) since boot or since the last disconnect, fire **one**
  "not found" notification; stay searching quietly.
- **CONNECTING:** deterministic tie-break so only one side dials — the device
  with the smaller `device_id` initiates; the other listens. Prevents duplicate
  links.
- **CONNECTED:** run a ping/pong heartbeat (default every 5s; link declared dead
  after 3 missed). On any failure, return to SEARCHING and reset the not-found
  latch so a fresh episode can notify again.
- **Backoff:** dialing retries use capped exponential backoff to avoid busy-loops
  while the peer is down.

**Sleep/wake handling:** the daemon subscribes to OS power events (macOS
`NSWorkspace` sleep/wake; Linux systemd-logind `PrepareForSleep` via D-Bus). On
wake it proactively tears down a stale connection and jumps straight to
SEARCHING, so reconnection starts immediately instead of waiting for a heartbeat
timeout.

### 4.6 Running at boot

- **Ubuntu:** a **systemd user service** (`~/.config/systemd/user/linker.service`)
  enabled with `systemctl --user enable --now linker` and
  `loginctl enable-linger` so it runs without an active graphical session.
  Feature backends that need the session bus (notifications) attach when the
  session is present.
- **macOS:** a **launchd LaunchAgent**
  (`~/Library/LaunchAgents/io.linker.daemon.plist`) with `RunAtLoad` and
  `KeepAlive`, so it starts at login and is restarted if it exits.

### 4.7 Configuration

A single human-readable config file in the per-OS config dir
(`~/.config/linker/config.toml` on Linux; `~/Library/Application Support/linker/`
on macOS): device name, enabled features, ports, peer allowlist, per-feature
options. The `device_id` and keys live beside it (keys in the OS keystore where
possible).

---

## 5. Security and privacy

- All traffic is TLS 1.3 (QUIC), authenticated by pinned per-device
  certificates — only paired devices can connect.
- Nothing leaves the LAN; there is no server and no telemetry.
- Each feature that touches sensitive data (clipboard, camera, mic) is
  individually toggleable and off until paired.
- OS permission prompts (camera, mic, accessibility/input) are surfaced to the
  user; the daemon degrades gracefully when a permission is denied (the feature
  reports unsupported instead of crashing).

---

## 6. Kernel and privileged-component strategy

This is the consolidated answer to "do we need kernel parts?" Each feature doc
repeats the relevant detail.

| Capability | Ubuntu (desktop) | macOS (laptop) |
|------------|------------------|----------------|
| Discovery, transport, pairing | userspace | userspace |
| Clipboard | userspace (Wayland/X11 or portal APIs) | userspace (NSPasteboard) |
| Notifications | userspace (D-Bus `org.freedesktop.Notifications`) | userspace (UNUserNotification) |
| DND | userspace (gsettings / D-Bus) | userspace (Shortcuts CLI; partial, see F2 doc) |
| KVM inject | **kernel `uinput`** (built-in module, already present) | userspace (CGEvent) |
| KVM capture | kernel evdev read (`/dev/input/event*`) | userspace (CGEventTap) |
| Camera sink | **kernel `v4l2loopback`** (existing out-of-tree module, via DKMS) | n/a (Mac is source) |
| Camera source | n/a | userspace (AVFoundation) |
| Mic sink | userspace **PipeWire** virtual node (kernel `snd-aloop` is an alternative) | n/a |
| Mic source | n/a | userspace (CoreAudio) |

**Conclusions:**

- **No custom kernel module is required for v1.** Ubuntu relies only on modules
  that already exist: `uinput` (ships with the kernel), `v4l2loopback`
  (installable via `apt`/DKMS), and PipeWire (userspace).
- **macOS kernel is never touched.** The Mac is the capture *source* and an
  input *target/source* via userspace frameworks; it needs no virtual devices
  and no kernel extension. This matches the project constraint that macOS kernel
  work is risky and to be avoided.
- **Optional future optimization:** a single bespoke Linker kernel module on
  Ubuntu could present camera + mic + input as one tightly integrated virtual
  device with lower overhead than gluing `v4l2loopback` + PipeWire + `uinput`
  together. This is explicitly **optional** and is also where the earlier
  Rust-for-Linux kernel-module learning would pay off. It is not on the critical
  path and should only be attempted after the userspace versions work.

---

## 7. Open questions

- Wi-Fi AP/client isolation will block peer-to-peer traffic on some networks;
  do we need a USB-C/Ethernet direct-link fallback?
- Should media use dedicated QUIC streams or QUIC datagrams? (Decided per media
  type in the camera/mic doc; may need measurement.)
- Clipboard on Wayland has restricted access outside the focused app; confirm the
  portal-based path covers our needs (see F1 doc).
- Trust store UX: how to re-pair cleanly if a device is reinstalled and its key
  changes.