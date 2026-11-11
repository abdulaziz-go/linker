# Linker — Feature Design: Software KVM

**Feature:** F3 · **Status:** Draft v1 · **Depends on:** Core (F0)

One keyboard and mouse control both machines. Move the pointer off the edge of
one screen and it continues on the other; keystrokes and clicks go to whichever
machine currently "owns" the cursor.

---

## 1. High-level design (how it works)

Think of the two screens as one wide desk arranged side by side (the user
defines the layout, e.g. "Mac is to the right of Ubuntu"). At any moment, one
machine is the **active** target. The machine with the physical keyboard/mouse
plugged in is the **server**; the other is the **client**.

1. The server constantly watches the local pointer position.
2. While the pointer is inside the server's own screen, nothing special happens —
   the OS handles input normally.
3. When the pointer hits the shared edge (e.g. the right edge), the server
   **captures** input: it stops the local OS from seeing the keyboard/mouse,
   hides the local cursor, and starts forwarding every event to the client over
   the network.
4. The client **injects** those events into its OS, so its own cursor moves and
   typing lands there.
5. When the pointer crosses back over the edge, the server releases capture and
   input returns to local handling.

Because only small event messages travel (not video), this is extremely light
and can feel instant on a LAN. There is no screen sharing — you look at each
machine's own monitor.

```
   Ubuntu screen (server)              Mac screen (client)
  ┌───────────────────────┐          ┌───────────────────────┐
  │                        │  edge    │                        │
  │      cursor ───────────┼────────▶ │ ───▶ injected cursor   │
  │                        │ crossing │                        │
  └───────────────────────┘          └───────────────────────┘
   captures kbd+mouse,                 receives events,
   forwards events ─────────────────▶  injects via OS API
```

Either machine can be the server; the roles are configurable. A natural default:
the Ubuntu desktop (with the real keyboard/mouse) is the server, the MacBook is
the client.

---

## 2. Reference prior art

This is a well-trodden pattern (Synergy, Barrier, Deskflow, and the Rust project
`lan-mouse`). The capture/inject split below — `CGEventTap`/`CGEvent` on macOS,
`evdev`+`uinput` on Linux — is the same approach those tools and cross-platform
input libraries use. We reimplement it inside the Linker core so it shares the
transport, pairing, and reliability with every other feature instead of being a
separate app.

---

## 3. Low-level design

### 3.1 Event model

A single compact message type on the **control plane** (reliable, ordered — input
must never arrive out of order or be lost):

```rust
pub enum InputMsg {
    // relative mouse motion while captured
    MouseMove { dx: i32, dy: i32 },
    MouseButton { button: u8, pressed: bool },
    MouseScroll { dx: i32, dy: i32 },
    Key { keycode: u32, pressed: bool },   // hardware-independent keycode
    // capture handoff
    Enter { edge: Edge, abs_ratio: f32 },  // pointer entered the client at this edge
    Leave,                                  // pointer left the client
    // keep-alive for the captured session
    Flush,
}
```

Key points:

- **Relative motion** (`dx/dy`) is sent while captured, not absolute coordinates,
  so the two screens' different resolutions/scaling don't fight. The `Enter`
  message carries the edge and a 0..1 ratio so the client places the cursor at
  the right height when it appears.
- **Keycodes are normalized** to a hardware-independent scancode set in
  `linker-proto`. The server maps its OS keycodes → Linker keycodes; the client
  maps Linker keycodes → its OS keycodes. This keeps Mac ↔ Linux key mapping
  correct (including modifiers) regardless of layout quirks.
- Modifier state is tracked explicitly and re-synced on every `Enter` so a stuck
  modifier can't leak across the handoff.

### 3.2 Server side — capture

**Ubuntu (server, default):**

- Read raw events from `/dev/input/event*` via **evdev** (Rust `evdev` crate).
- To stop the local desktop from also reacting while captured, **grab** the
  devices with `EVIOCGRAB` (exclusive access). On release, ungrab.
- Pointer-edge detection: track accumulated motion; on Wayland, absolute pointer
  position isn't freely readable, so the server maintains its own virtual cursor
  position from relative deltas and compares against screen geometry.

**macOS (if the Mac is the server instead):**

- Install a **`CGEventTap`** at the session level to observe (and, while
  captured, suppress) keyboard and mouse events.
- Requires **Accessibility** and **Input Monitoring** permissions (one-time
  prompt in System Settings → Privacy & Security).

### 3.3 Client side — inject

**Ubuntu (client):**

- Create a virtual input device with the kernel **`uinput`** module (Rust
  `uinput`/`input-linux` crate) exposing keyboard + relative pointer + buttons +
  scroll. Injected events look to the OS exactly like a real USB device.
- Needs access to `/dev/uinput` (udev rule granting the `input` group, or the
  daemon runs with the right capability).

**macOS (client, default):**

- Synthesize events with **`CGEvent`** (Quartz Event Services): `CGEventCreateMouseEvent`,
  `CGEventCreateKeyboardEvent`, scroll events, posted with `CGEventPost`.
- Requires **Accessibility** permission.

### 3.4 Capture handoff protocol

```
server: pointer reaches shared edge
   └─▶ grab local devices, hide local cursor
   └─▶ send Enter{edge, ratio}
   └─▶ for each local event: translate → send InputMsg on control stream
client: on Enter  → show/position cursor, begin injecting
client: on events → inject via uinput/CGEvent
server: pointer returns across edge
   └─▶ send Leave, ungrab devices, restore local cursor
client: on Leave  → stop injecting, park cursor
```

Edge geometry (which side, screen sizes, vertical offset) comes from the config
layout and is exchanged once at connect.

### 3.5 Reliability and safety

The overriding invariant: **the user must never be locked out — any problem
returns input to the server immediately.** The handoff is therefore gated and
acknowledged, not fire-and-forget, and the grab is wrapped in an RAII guard that
ungrabs on every exit path (normal, task error/panic, shutdown).

- **Readiness gating:** the client advertises `KvmReady{ready}` and the server
  **never hands off unless the client has affirmatively said it can inject**
  (macOS: Accessibility trusted and event posting works). The client re-checks
  at runtime and re-advertises on every change, so a missing/added permission is
  reflected within ~2 s. *(This closes the original freeze: a Mac that lost
  Accessibility after a rebuild still advertised the KVM feature, so the server
  grabbed and forwarded into a void. Advertising the role is no longer the same
  as being ready.)*
- **Handoff acknowledgement:** after `Enter` the server waits ~400 ms for an
  explicit `KvmAck` and **grabs the devices only after the ack**. No ack → the
  handoff is aborted, input stays local, and a banner explains why.
- **Liveness while captured:** the client sends a periodic `KvmHeartbeat`
  (~250 ms) and a `KvmError{reason}` on any injection failure. The server
  releases if heartbeats stop for ~1 s or an error is reported.
- **Dead-man's switch:** if the control link drops while captured, the server
  immediately **ungrabs** and restores the local cursor. The client clears any
  held keys/buttons on `Leave` or link loss (prevents a "stuck key").
- **Client-independent return:** the server tracks the virtual cursor itself, so
  moving back across the edge returns control even if the client never sends
  `Return`.
- **Panic hotkey:** a configurable hotkey (default Ctrl+Alt+Shift+Esc, matching
  either hand) is detected straight from the evdev stream while grabbed,
  independent of link and peer state, and force-releases capture.
- **Notifications:** every forced release shows a desktop banner naming the
  reason (e.g. "KVM released: Mac is not accepting input (Accessibility
  permission missing)").
- **Ordering:** input rides reliable, ordered QUIC streams; no datagrams here.
- **Modifier resync:** full modifier state is sent on `Enter` and cleared on
  `Leave`.

New control messages (client → server): `KvmReady{ready}`, `KvmAck`,
`KvmHeartbeat`, `KvmError{reason}`.

### 3.6 Performance

- Messages are tiny (a few bytes); the cost is per-event syscalls and network
  RTT. On a quiet LAN this is sub-millisecond to low-millisecond — below human
  perception for pointer motion.
- Coalesce rapid `MouseMove` deltas within a single event-loop tick to avoid
  flooding the stream during fast motion.
- Pin the input path to its own QUIC stream so a busy media stream (camera) can't
  delay a keystroke (QUIC prevents head-of-line blocking across streams).

---

## 4. Kernel / privileged components

- **Ubuntu:** uses the built-in **`uinput`** kernel module (inject) and evdev
  device reads with `EVIOCGRAB` (capture). Both are standard kernel facilities —
  **no custom module**. Setup is a udev rule + group membership.
- **macOS:** entirely userspace (`CGEventTap` / `CGEvent`), gated by
  Accessibility / Input Monitoring permissions. No kernel extension.

---

## 5. Permissions checklist

- **Ubuntu:** daemon can read `/dev/input/event*` and write `/dev/uinput`
  (udev rules; `input` group). `uinput` module loaded at boot.
- **macOS:** Accessibility + Input Monitoring granted to `linkerd`.

---

## 6. Open questions

- Wayland limits global pointer warping and absolute position; confirm the
  virtual-cursor-from-deltas approach is robust across GNOME versions, or use a
  compositor portal if one becomes available.
- Multi-monitor on either side: how edges map when a machine has several
  displays.
- Clipboard-follows-cursor: should a copy while on the client auto-sync (ties
  into F1)?