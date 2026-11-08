# Linker — Product Requirements Document (PRD)

**Codename:** Linker
**Owner:** Abdulaziz
**Status:** Draft v1
**Last updated:** 2026-11-08

---

## 1. One-paragraph summary

Linker makes a MacBook and an Ubuntu desktop behave like one connected
workspace over local Wi-Fi. Copy on one, paste on the other. See your Mac's
notifications on Ubuntu. Turn on Do Not Disturb once and both go quiet. Drive
both screens with a single keyboard and mouse. Use the MacBook's camera and
microphone inside any Ubuntu app as if they were plugged into the Ubuntu
machine. Everything runs peer-to-peer on the local network, with no cloud and
no account.

---

## 2. Why build this

Apple already gives Mac + iPhone a tight ecosystem: Universal Clipboard,
notification mirroring, Continuity Camera, Handoff. Linux is left out. Existing
cross-platform tools (e.g. KDE Connect) cover some of this but, in practice,
drop the connection after sleep/unlock, don't sync media on the clipboard, and
don't offer camera/microphone sharing or a real software KVM. Linker is a
purpose-built system for one person's exact setup — one MacBook, one Ubuntu
desktop, same Wi-Fi — optimized for **reliability** and the **specific
features that matter**, instead of being a general-purpose tool.

---

## 3. Who it's for

A single user (the author) with:

- A MacBook (macOS, recent version) — the "laptop".
- An Ubuntu desktop (recent LTS, GNOME) — the "desktop".
- Both on the same local Wi-Fi/LAN most of the time.
- Willingness to install a background service and grant OS permissions on both.

Multi-user, enterprise, mobile (Android/iOS clients), and internet-relay
scenarios are explicitly **out of scope** for v1.

---

## 4. Goals and non-goals

### Goals

1. **Reliability first.** The link survives sleep, lid-close, Wi-Fi blips, and
   screen lock/unlock, and re-establishes itself automatically. If the peer is
   genuinely unreachable, the user gets exactly one clear notification
   ("MacBook not found"), not silence and not spam.
2. **Starts itself.** Runs as a background service that launches at boot/login
   on both machines. The user never has to "open the app".
3. **The five features** below, working well, in priority order.
4. **Feels instant.** Clipboard and input actions should feel immediate
   (well under a human-noticeable delay). Camera/mic should be usable for video
   calls (low enough latency, stable frame rate).
5. **Private by design.** All traffic encrypted and restricted to paired
   devices on the LAN.

### Non-goals (v1)

- Screen sharing / remote desktop. (Explicitly dropped.)
- File-manager-level folder sync (basic file send may come later).
- Android/iOS apps. (The iPhone is handled *by Apple* up to the Mac; Linker only
  bridges Mac ↔ Ubuntu.)
- Working over the internet / outside the LAN.
- Modifying the macOS kernel.

---

## 5. Features and priority

Priority uses MoSCoW (Must / Should / Could).

| # | Feature | Priority | One-line description |
|---|---------|----------|----------------------|
| F0 | **Core / Common package** | Must | Shared cross-platform engine: discovery, secure transport, pairing, daemon lifecycle. Everything else depends on it. |
| F1 | **Universal Clipboard** | Must | Copy text, images, and files on one device; paste on the other. Extends the iPhone → Mac chain so the same content reaches Ubuntu. |
| F2 | **Notifications + Do Not Disturb** | Must | Mac notifications appear on Ubuntu; toggling DND/Focus on either device sets both. |
| F3 | **Software KVM** | Should | One keyboard and mouse control both screens; the cursor crosses the screen edge between machines. |
| F4 | **Camera & Microphone sharing** | Should (flagship) | The MacBook's camera and mic appear on Ubuntu as a standard webcam and mic, usable in any app (Zoom, browser, OBS). |

> F4 is the hardest and the most distinctive. It is a "should" only because F0–F2
> must land first; in terms of product value it is the headline feature.

Each feature has a dedicated design document (see the index). The remainder of
this PRD stays at product level.

---

## 6. What each feature means to the user

**Universal Clipboard.** The user copies a paragraph, a screenshot, or a file
on the Mac. On Ubuntu, Ctrl+V pastes it — text as text, image as image, file as
a real file. Because the Mac already receives the iPhone's clipboard through
Apple's Universal Clipboard, whatever the user copied on the iPhone is also
available to paste on Ubuntu, one hop later. The reverse (copy on Ubuntu →
paste on Mac) works too.

**Notifications + Do Not Disturb.** A message arrives on the Mac; a matching
banner appears on Ubuntu. When the user turns on Do Not Disturb (macOS Focus or
GNOME's "Do Not Disturb"), the other machine goes quiet as well, and turning it
off anywhere clears it everywhere. The intent: one mental switch for "leave me
alone," honored across both desktops.

**Software KVM.** The user keeps one keyboard and mouse on the desk. Moving the
pointer off the right edge of the Ubuntu screen makes it appear on the Mac
screen (or wherever the layout places it), and keystrokes go to whichever
machine the pointer is on. It feels like a multi-monitor setup that happens to
span two computers.

**Camera & Microphone sharing.** The MacBook has the better camera and mic. The
user joins a meeting *on Ubuntu*, opens the app's device picker, and selects
"Linker Camera" and "Linker Microphone." The live feed from the MacBook's camera
and mic flows in. To the Ubuntu app, these look exactly like a physical webcam
and microphone.

---

## 7. Success criteria

The product is "good enough to live with" when:

- After a cold boot of both machines, they find each other and connect within
  ~15 seconds with no user action.
- Closing the MacBook lid and reopening it restores the link automatically,
  with no manual restart, within a few seconds of wake.
- Clipboard propagation (text/image) is perceptually instant on the same Wi-Fi.
- DND toggled on one device reflects on the other within ~1 second.
- KVM pointer handoff has no visible lag during normal desk use.
- The shared camera sustains at least 720p/30fps with latency low enough for a
  natural video call, and the mic stays in sync with it.
- When the peer is off/asleep/unreachable, the user sees one "not found"
  notice and the service keeps trying quietly in the background.

---

## 8. Constraints and assumptions

- **Same LAN.** Both devices are assumed reachable on the same subnet. (Wi-Fi
  "client isolation" / AP isolation would block peer-to-peer traffic; documented
  as a known environmental blocker.)
- **OS permissions required.** macOS will require Accessibility (for input),
  Camera, Microphone, Screen/Input Monitoring, and Notification permissions;
  Ubuntu will require membership in `input`/`video` groups and the
  `v4l2loopback` module installed. These are one-time setup steps.
- **macOS limitations are real.** Some macOS capabilities (notably programmatic
  control of Focus/DND) have no official public API; the design documents call
  out where we rely on supported-but-indirect mechanisms and where behavior may
  be partial. See the Notifications/DND design doc.
- **Rust toolchain** on both machines for building; end users get a built
  binary + service installer.

---

## 9. High-level architecture (product view)

```
        MacBook (laptop)                         Ubuntu desktop
   +-------------------------+              +-------------------------+
   |   linker daemon (Rust)   |              |   linker daemon (Rust)   |
   |  +-------------------+  |   Wi-Fi /    |  +-------------------+  |
   |  |   core package    |<-+---- LAN ----+->|   core package    |  |
   |  | discovery/transport| | (encrypted) |  | discovery/transport| |
   |  +-------------------+  |              |  +-------------------+  |
   |  platform backends:     |              |  platform backends:     |
   |   clipboard, notif,     |              |   clipboard, notif,     |
   |   input, capture        |              |   input, virtual devices|
   +-------------------------+              +-------------------------+
```

The same Rust core runs on both sides; only the thin "platform backend" layer
differs per OS. Features are modules that exchange typed messages over the
secure transport. Details in [01-architecture-core.md](01-architecture-core.md).

---

## 10. Kernel / privileged components (product view)

Most of Linker is ordinary userspace software. A few features need privileged OS
hooks, summarized here and detailed per feature:

- **Ubuntu:** uses **existing** kernel facilities — `uinput` (synthesize
  keyboard/mouse for KVM), `v4l2loopback` (present the Mac camera as
  `/dev/videoN`), and PipeWire (present the Mac mic as an input source). No
  custom kernel module is required for v1. A bespoke Linker kernel module is
  recorded as an **optional** future optimization, not a dependency.
- **macOS:** no kernel changes. Input, capture, and notifications are all done
  through userspace frameworks. (This matches the constraint that macOS kernel
  modification is risky and to be avoided.)

---

## 11. Roadmap / milestones

| Milestone | Delivers | Why this order |
|-----------|----------|----------------|
| **M0 — Link** | Core package: discovery, secure transport, pairing, reliable auto-reconnect, "not found" notice, boot services on both OSes. | Nothing works without a trustworthy link. |
| **M1 — Clipboard** | Universal Clipboard (text, image, files), both directions. | Highest daily value, exercises the whole message path. |
| **M2 — Notifications + DND** | Notification mirroring + bidirectional DND. | Builds on the link; mostly userspace. |
| **M3 — Software KVM** | Keyboard/mouse across both screens with edge handoff. | Needs low-latency input path; independent of media. |
| **M4 — Camera + Mic** | Mac camera/mic as Ubuntu devices. | Hardest; depends on a solid transport and the virtual-device setup. |

---

## 12. Glossary

- **Peer / device:** one of the two machines (laptop = MacBook, desktop = Ubuntu).
- **Daemon:** the always-on background service (`linkerd`).
- **Core package:** the shared Rust library used identically on both OSes.
- **Platform backend:** the per-OS code that implements a capability (e.g.
  reading the clipboard) behind a common interface.
- **Control channel:** the reliable, low-rate channel for commands and small payloads.
- **Media stream:** a high-rate channel for continuous data (audio/video/input).
- **KVM:** Keyboard-Video-Mouse; here, sharing keyboard and mouse (no video) across machines.
- **DND:** Do Not Disturb (macOS "Focus", GNOME "Do Not Disturb").
- **Virtual device:** an OS-level fake device (camera/mic) that apps treat as real.