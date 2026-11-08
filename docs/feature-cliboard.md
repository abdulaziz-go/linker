# Linker — Feature Design: Universal Clipboard

**Feature:** F1 · **Status:** Draft v1 · **Depends on:** Core (F0)

Copy on one device, paste on the other — text, images, and files. Because the
MacBook already receives the iPhone's clipboard through Apple's Universal
Clipboard, anything copied on the iPhone becomes paste-able on Ubuntu one hop
later (iPhone → Mac → Ubuntu), and copies made on Ubuntu become available on the
Mac.

---

## 1. High-level design (how it works)

Each device watches its own clipboard. When the clipboard changes, the device
sends the new content to the peer, which writes it into *its* clipboard. From the
user's point of view, the two clipboards are always the same.

```
   iPhone ──(Apple Universal Clipboard)──▶ Mac clipboard
                                              │  Linker sees change
                                              ▼
                                      send to Ubuntu ──▶ Ubuntu clipboard
                                              ▲
                                              │  Linker sees change
                                      Ubuntu copy ◀── user copies on Ubuntu
```

Linker does **not** talk to the iPhone. The iPhone → Mac leg is Apple's; Linker
owns only the Mac ↔ Ubuntu leg. That single leg is enough to complete the chain
in both directions.

Content types supported:

- **Text** (plain and rich/HTML where available).
- **Images** (screenshots, copied pictures) as PNG.
- **Files** (one or more) — the file list is copied; the bytes are transferred so
  a paste on the other side produces real files.

---

## 2. The sync loop and the echo problem

The core challenge in any clipboard sync is avoiding an infinite loop: device A
writes what it received from B, which looks like a new change, which it sends
back to B, forever. Linker prevents this with **content fingerprinting + origin
tagging**:

1. Every clipboard payload gets a content hash and a monotonically increasing
   `seq` with the originating `device_id`.
2. When a device writes a received payload into its own clipboard, it records
   that hash as "just applied."
3. The clipboard watcher ignores the next change if its hash matches the
   "just applied" value. Only genuinely new content is propagated.

This also deduplicates the case where both clipboards already hold the same thing.

---

## 3. Low-level design

### 3.1 Messages

Small control messages announce clipboard changes; large payloads (images,
files) are streamed so they don't block the control channel.

```rust
pub enum ClipboardMsg {
    // announce: metadata only
    Offer {
        seq: u64,
        origin: DeviceId,
        kinds: Vec<ClipKind>,      // Text | Html | Image | Files
        hash: [u8; 32],            // content fingerprint
        total_bytes: u64,
    },
    // inline small text
    Text { seq: u64, text: String, html: Option<String> },
    // request the heavy payload for an Offer we want
    Pull { seq: u64 },
    // file-list metadata (bytes follow on a dedicated stream)
    Files { seq: u64, files: Vec<FileMeta> },  // name, size, mime
}
pub struct FileMeta { pub name: String, pub size: u64, pub mime: String }
```

- **Small text** is sent inline immediately (lowest latency for the common case).
- **Images and files** are advertised via `Offer`; the receiver pulls them
  (`Pull`) and the bytes arrive on a **dedicated QUIC stream**, so a 20 MB image
  doesn't stall a keystroke or a notification. A size threshold (e.g. 256 KB)
  decides inline vs. offer/pull.
- Very large clipboards are capped by a configurable limit; above it, the file
  path/offer is kept but auto-transfer can be deferred until paste (lazy pull).

### 3.2 macOS backend (NSPasteboard)

- **Read/write:** `NSPasteboard.general` with typed items
  (`public.utf8-plain-text`, `public.html`, `public.png`, `public.file-url`).
- **Change detection:** `NSPasteboard` exposes a `changeCount` that increments on
  every change but has **no change notification**, so the backend **polls**
  `changeCount` on a short interval (e.g. 250–500 ms) and reads only when it
  moves. Polling is cheap and is the standard approach on macOS.
- **iPhone chain:** nothing special — when the iPhone's content lands in the
  Mac's pasteboard via Universal Clipboard, `changeCount` moves and Linker picks
  it up like any other change. (Note: Apple's Universal Clipboard itself may have
  a short delay / require the item to be pulled; Linker simply reflects whatever
  the Mac pasteboard currently holds.)
- Accessing the pasteboard is userspace and needs no special entitlement for the
  common types; file URLs are read within the app sandbox rules.

### 3.3 Ubuntu backend (Wayland/X11)

Clipboard access on Linux depends on the display server:

- **X11:** straightforward via the selection mechanism (`CLIPBOARD` selection);
  Rust crates like `arboard` or `wl-clipboard`-style helpers work.
- **Wayland (GNOME default):** for security, clipboard reads are restricted to
  the focused client. Two supported paths:
  - the **`wlr-data-control`** protocol (lets a background app read/write the
    clipboard without focus) where the compositor supports it; or
  - the **XDG Desktop Portal** clipboard interface.
  GNOME's support for background clipboard access varies by version, so the
  backend probes at startup and reports the capability; on a restricted
  compositor, clipboard sync may require the Linker helper to have an active
  session. This is called out as an **open risk** below.
- **Change detection:** event-driven where the protocol provides it (selection
  ownership changes), else a short poll like macOS.
- **Images/files:** images as `image/png`; files via `text/uri-list` for the
  list plus byte transfer into a staging directory so paste yields real files.

### 3.4 Flow example (image copied on Mac → paste on Ubuntu)

```
Mac: user copies a screenshot
 └─ changeCount moves → backend reads PNG from NSPasteboard
 └─ hash it; send Offer{kinds:[Image], hash, total_bytes}
Ubuntu: receives Offer → not seen before → send Pull{seq}
Mac: opens a dedicated QUIC stream, sends PNG bytes
Ubuntu: receives bytes → write image/png into clipboard, record hash as "applied"
 └─ user pastes → real image appears
```

---

## 4. Performance & optimization

- Inline text path keeps the everyday case at near-zero latency.
- Heavy payloads use their own QUIC stream (no head-of-line blocking) and are
  **pulled on demand**, so copying a huge file doesn't force a transfer the user
  never pastes (optional lazy mode).
- Content hashing dedups and breaks echo loops cheaply.
- Compression (e.g. `zstd`) on image/file streams where it helps.

---

## 5. Kernel / privileged components

**None.** Universal Clipboard is entirely userspace on both sides
(`NSPasteboard` on macOS; selection/portal APIs on Ubuntu). No kernel module,
no special privilege beyond normal desktop clipboard access.

---

## 6. Security & privacy

- Clipboards often hold passwords; all payloads travel only over the paired,
  encrypted QUIC link.
- Optional setting: don't auto-sync items marked "transient/concealed" (password
  managers set a pasteboard flag on macOS: `org.nspasteboard.ConcealedType`).
- Per-direction and per-type toggles (e.g. sync text but not files).

---

## 7. Open questions

- Wayland background clipboard access on the user's GNOME version — confirm
  `wlr-data-control` vs. portal availability; this determines whether sync works
  without an active focused helper.
- Rich text fidelity (HTML/RTF) across the two platforms — how much to normalize.
- Should file transfers land in a fixed "Linker received" folder or a temp dir
  that's cleaned up after paste?
- Respecting Apple's concealed-type flag end to end.