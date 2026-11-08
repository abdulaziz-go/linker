# Linker — Feature Design: Notifications & Do Not Disturb

**Feature:** F2 · **Status:** Draft v1 · **Depends on:** Core (F0)

Two parts that belong together:

1. **Notification mirroring** — notifications that appear on the Mac also appear
   on Ubuntu (and optionally the reverse).
2. **Do Not Disturb sync** — turning DND/Focus on or off on either machine sets
   the same state on the other, so "leave me alone" is one switch for both
   desktops.

---

## 1. High-level design (how it works)

### Notifications

When an app shows a notification on the Mac, Linker captures its title, body, and
app name, sends it to Ubuntu, and Ubuntu pops a native notification with the same
text. The user sees Mac alerts on the Ubuntu screen without looking at the
MacBook.

### Do Not Disturb

Each device watches its own DND/Focus state. When it changes, it tells the peer,
and the peer applies the same state. A hash/origin guard (same idea as the
clipboard) prevents a toggle from bouncing back and forth.

```
  Mac notification appears ──▶ capture ──▶ send ──▶ Ubuntu shows banner

  DND turned ON (either side) ──▶ send Dnd{on:true} ──▶ other side turns DND ON
  DND turned OFF (either side) ─▶ send Dnd{on:false} ─▶ other side turns DND OFF
```

When DND is on, Linker suppresses mirrored banners (it still records them, but
stays quiet), matching what the user expects from "do not disturb."

---

## 2. Low-level design

### 2.1 Messages

```rust
pub enum NotificationMsg {
    Show {
        id: u64,
        app: String,         // originating app name
        title: String,
        body: String,
        // optional: app icon bytes or a known icon id (sent once, cached)
        icon: Option<IconRef>,
        timestamp: u64,
    },
    Dismiss { id: u64 },     // best-effort: peer dismisses the mirrored copy
}

pub enum DndMsg {
    Set { on: bool, origin: DeviceId, seq: u64 },
}
```

Notifications and DND ride the **control plane** (reliable). They are small and
infrequent.

### 2.2 Notification capture — macOS (source)

This is the hard part on macOS, and the design is honest about it:

- macOS does **not** offer a public API to read *other apps'* notifications. The
  supported options, in order of preference:
  1. **Notification database observation:** macOS stores delivered notifications
     in a per-user database managed by `usernoted`
     (`~/Library/.../com.apple.notificationcenter/db2/db`). A background helper
     can watch this SQLite DB for new rows and extract app/title/body. This is
     what several menu-bar "notification mirroring" tools do. It is
     **unofficial** and can change between macOS releases, so it is isolated
     behind the backend and version-guarded.
  2. **Accessibility observation** of the Notification Center UI (read banners as
     they appear via the Accessibility API). Also unofficial, more fragile.
- Linker will implement path (1) as primary, (2) as fallback, and treat the whole
  capability as "best-effort, may degrade after a macOS update." The PRD already
  flags macOS limitations; this is the main one for F2.
- Linker's **own** notifications (e.g. "MacBook connected") always use the
  official `UNUserNotification` API.

### 2.3 Notification display — Ubuntu (sink)

- Show via the standard **D-Bus** service
  `org.freedesktop.Notifications` (`Notify` method) — this is the native
  notification path GNOME uses. Rust: `notify-rust` or a direct `zbus` call.
- App name and icon are passed through; icons are cached by hash so they aren't
  resent each time.
- Dismissals are best-effort (`CloseNotification`).

### 2.4 DND detection and control

**Ubuntu (GNOME):**

- **State is readable and writable** via GSettings:
  `org.gnome.desktop.notifications show-banners` (false = DND on). Read/write with
  `gsettings`/`dconf` or the GIO `Settings` API (Rust `gio`), and subscribe to
  its `changed` signal for instant detection. This is clean and reliable.

**macOS (Focus / Do Not Disturb):**

- **This is a known limitation.** macOS has **no official public API** to turn
  Focus/DND on or off, and detection changed across versions. The design uses:
  - **Setting DND:** invoke a user-created **Shortcut** via the `shortcuts run`
    CLI (the Shortcuts app has "Set Focus" actions). Linker ships setup
    instructions to create two shortcuts ("Linker DND On" / "Linker DND Off"); the
    daemon runs them. This is Apple-supported automation, if indirect.
  - **Detecting DND:** read the Focus/assertion state that macOS writes to a
    user plist (historically
    `~/Library/DoNotDisturb/DB/Assertions.json` / `ModeConfigurations`, path
    varies by version) and/or watch for Focus change via a Shortcuts automation
    that pokes the daemon. Because the detection path is version-sensitive, it is
    isolated and version-guarded, like notification capture.
- **Consequence for sync direction:** Ubuntu→Mac and Mac→Ubuntu both work for
  *applying* DND (Shortcuts can set it); *detecting* a manual Focus toggle on the
  Mac is the fragile part. If detection is unavailable on a given macOS version,
  Linker degrades to "DND set from Ubuntu propagates to Mac" plus a manual Mac
  toggle, and documents that clearly.

### 2.5 Echo/loop prevention

Same mechanism as clipboard: each `Dnd::Set` carries `origin` + `seq`; when a
device applies a received state it records it as "just applied" and its own
watcher ignores the matching change, so the toggle doesn't ping-pong.

---

## 3. Performance & optimization

- Tiny, infrequent messages — latency is dominated by OS detection (GSettings
  signal is immediate; macOS plist watching is near-immediate; Shortcuts
  execution adds a fraction of a second).
- Icons cached by hash to avoid resending.
- When DND is on, mirrored notifications are stored but not shown (configurable:
  "silent log" vs. "fully drop").

---

## 4. Kernel / privileged components

**None.** Everything is userspace:

- Ubuntu: D-Bus notifications + GSettings — no kernel, no special privilege.
- macOS: notification DB/Accessibility observation + Shortcuts CLI — userspace,
  gated by user permissions (Automation/Accessibility, Full Disk Access may be
  needed to read the notification DB).

---

## 5. Permissions checklist

- **macOS:** permission to read the notification database (may require Full Disk
  Access for `linkerd`), Automation permission to run Shortcuts, optionally
  Accessibility for the UI-observation fallback.
- **Ubuntu:** normal session bus access (default); no extra privilege.

---

## 6. Open questions

- Which macOS versions expose which Focus plist path — needs a compatibility
  table and graceful fallback per version.
- Should mirrored notifications be actionable (dismiss/reply) or display-only in
  v1? (v1: display + best-effort dismiss.)
- Rate-limiting / coalescing noisy apps so Ubuntu isn't flooded.
- Privacy filter: per-app allow/deny list for which notifications mirror.