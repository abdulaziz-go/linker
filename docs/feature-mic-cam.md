# Linker — Feature Design: Camera & Microphone Sharing

**Feature:** F4 (flagship) · **Status:** Draft v1 · **Depends on:** Core (F0)

The MacBook's camera and microphone become usable on Ubuntu as a **standard
webcam and microphone**. Open any Ubuntu app (Zoom, Google Meet in the browser,
OBS), pick "Linker Camera" and "Linker Microphone" in its device list, and you get
the live MacBook camera and mic. The apps don't know or care that the real
hardware is on another machine.

This is the hardest feature: it is a continuous, low-latency, synchronized
audio+video pipeline across two machines.

---

## 1. High-level design (how it works)

The MacBook is the **source** (it has the camera and mic). The Ubuntu desktop is
the **sink** (it presents fake devices that other apps read).

```
  MacBook (source)                                   Ubuntu (sink)
  ┌───────────────────────────┐                      ┌───────────────────────────────┐
  │ camera ─▶ capture ─▶ encode│     Wi-Fi / LAN      │ decode ─▶ /dev/video42 (v4l2   │
  │          (AVFoundation)    │  ── video stream ──▶ │          loopback)  ← apps read │
  │          (VideoToolbox)    │                      │                                │
  │ mic ────▶ capture ─▶ encode│  ── audio stream ──▶ │ decode ─▶ virtual mic (PipeWire│
  │          (CoreAudio)(Opus) │                      │          source)    ← apps read │
  └───────────────────────────┘                      └───────────────────────────────┘
```

On Ubuntu, the magic is the **virtual devices**: a loopback video device that
looks exactly like a webcam, and a virtual microphone node. Linker writes the
decoded frames/samples into them; every app that enumerates cameras/mics sees
them as real.

"Main for both devices" means: the Mac keeps using its own camera/mic natively
(nothing changes there), and the *same* physical camera/mic is simultaneously
available on Ubuntu through this pipeline.

---

## 2. Low-level design — Video

### 2.1 Capture (macOS)

- **API:** AVFoundation `AVCaptureSession` with the camera as input and a
  `AVCaptureVideoDataOutput` delivering frames (`CMSampleBuffer`, typically
  NV12/420v pixel format).
- **Permissions:** Camera permission (one-time prompt). If the camera is in use
  by another Mac app, macOS allows sharing in modern versions; otherwise the
  session reports busy.
- Target capture format negotiated from the sink's request (e.g. 1280×720 @ 30).

### 2.2 Encode (macOS)

- **Hardware H.264 (or HEVC) via VideoToolbox.** VideoToolbox gives
  hardware-accelerated encoding on Apple silicon/Intel Macs, which keeps CPU low
  and latency small. Configure for **low-latency**: real-time mode, no B-frames,
  short/keyframe-on-demand GOP, constrained bitrate (e.g. 2–6 Mbps for 720p30).
- Output is an Annex-B / AVCC H.264 elementary stream chunked per frame.

### 2.3 Transport

- Video rides a **dedicated media channel** on the QUIC connection. Two options,
  decided by measurement:
  - **QUIC unidirectional stream per GOP/segment** (reliable, ordered) — simplest,
    good on a clean LAN.
  - **QUIC datagrams** (unreliable) for frame slices — better worst-case latency
    because a late/lost packet is skipped, not retransmitted; needs a lightweight
    framing + "wait for next keyframe on loss" policy.
- Each frame is tagged with a **capture timestamp** (monotonic, shared clock
  offset negotiated at start) for A/V sync and jitter handling.
- Because QUIC multiplexes without head-of-line blocking, video never stalls
  clipboard/KVM/notifications and vice versa.

### 2.4 Decode + present (Ubuntu)

- **Decode:** H.264 via hardware VAAPI where available (Rust via `ffmpeg`/`vaapi`
  bindings or GStreamer), else software.
- **Present into a virtual camera** so every app sees a webcam:
  - **Primary: `v4l2loopback`.** A kernel module that creates a
    `/dev/videoN` device apps can open like a real camera. Linker creates one
    device (e.g. `/dev/video42`, card label "Linker Camera") and writes decoded
    frames into it (raw YUV/`YUYV`/NV12). This is the classic, maximally
    compatible path (OBS, Chrome, Zoom all accept a v4l2loopback device).
  - **Alternative/complement: PipeWire camera.** Newer stacks expose virtual
    cameras through PipeWire (the "PipeWire is the new v4l2loopback" direction).
    PipeWire is userspace and integrates with the portal camera API, but
    app-by-app support is still maturing, so v1 uses **v4l2loopback** for
    compatibility and keeps PipeWire as a forward-looking option.
- A small **jitter buffer** (tens of ms) before the virtual device smooths
  network jitter; it is kept minimal to protect latency.

---

## 3. Low-level design — Audio

### 3.1 Capture + encode (macOS)

- **Capture:** CoreAudio / AVFoundation audio input from the mic (PCM, e.g.
  48 kHz).
- **Encode:** **Opus** — the standard low-latency voice/music codec (Rust
  `opus` / `audiopus`). Small frames (e.g. 10–20 ms), ~24–64 kbps mono/stereo.
  Opus is chosen for low latency, good quality, and built-in resilience options.

### 3.2 Transport

- Audio rides its **own** media channel (separate from video). Audio is more
  latency- and continuity-sensitive than video, so it typically uses
  **datagrams** with a tiny jitter buffer and packet-loss concealment (Opus
  handles minor loss gracefully).
- Each audio frame carries a timestamp from the **same shared clock** as video
  for sync.

### 3.3 Present into a virtual mic (Ubuntu)

- **Primary: PipeWire virtual source** (a "null sink" + source, or a
  `module-pipe-source`-style node) labeled "Linker Microphone." Decoded Opus → PCM
  is written into this node; apps that enumerate microphones see it and can
  select it. PipeWire is the modern Ubuntu audio server and is **userspace** — no
  kernel module needed.
- **Alternative:** the ALSA `snd-aloop` kernel module provides a loopback sound
  device if a kernel-level path is ever preferred; not needed for v1.

---

## 4. Audio/Video synchronization

- At stream start, the two daemons negotiate a **clock offset** (exchange
  monotonic timestamps, estimate one-way delay) so Mac capture timestamps can be
  compared on Ubuntu.
- Video frames and audio frames are timestamped at capture.
- On the sink, a shared scheduler releases audio and video to their virtual
  devices according to timestamps, keeping lip-sync within a target (e.g.
  < 50 ms skew). The jitter buffers for both are coordinated.
- If the network degrades, video degrades first (drop frames / lower bitrate) to
  protect audio continuity.

---

## 5. Performance & optimization

- **Hardware encode (VideoToolbox) + hardware decode (VAAPI)** keep CPU low and
  latency small on both ends.
- **Dynamic bitrate:** the sink reports loss/latency back on a control stream;
  the Mac encoder adjusts bitrate and requests keyframes after loss (classic
  congestion response).
- **Keyframe-on-loss:** on packet loss with datagram mode, the sink requests a
  fresh keyframe rather than showing corruption.
- **Resolution/fps negotiation:** the sink asks for what the consuming app needs
  (e.g. 720p30 for calls); don't capture/encode more than required.
- **Minimal jitter buffers** sized to the measured LAN jitter, not a fixed large
  value.
- Separate QUIC channels for audio vs. video vs. control so none blocks another.
- **Latency budget (target, 720p30 on quiet Wi-Fi):** capture+encode (Mac,
  hardware) ~10–20 ms, network ~1–10 ms, decode+present (Ubuntu) ~10–20 ms, plus
  jitter buffer ~10–30 ms → roughly 40–80 ms glass-to-glass, acceptable for video
  calls. To be validated by measurement.

---

## 6. Kernel / privileged components

This is the one feature with a real kernel touchpoint — on Ubuntu only.

- **Ubuntu — video:** **`v4l2loopback`**, an existing out-of-tree kernel module
  installed via `apt`/DKMS (`sudo apt install v4l2loopback-dkms`). Linker loads it
  (one device, named "Linker Camera") and writes frames to `/dev/videoN`. **No
  custom module is written** — we use the maintained community module. Setup:
  module load at boot (modprobe config) + the daemon having write access to the
  device node.
- **Ubuntu — audio:** **PipeWire** virtual source — **userspace, no kernel
  module.** (`snd-aloop` is an optional kernel alternative, not used in v1.)
- **macOS:** **no kernel work at all.** Capture (AVFoundation), encode
  (VideoToolbox), audio (CoreAudio), all userspace, gated by Camera/Microphone
  permissions. The Mac is the source and needs no virtual devices. This honors
  the constraint that the macOS kernel must not be modified.

**Optional future optimization (explicitly not required):** replace the
`v4l2loopback` + PipeWire combination with a single bespoke **Linker kernel
module** on Ubuntu that presents camera + mic (and possibly KVM input) as one
integrated virtual device, potentially shaving a copy/context-switch and
simplifying setup. This is the natural place the earlier Rust-for-Linux
kernel-module work could be applied. It should only be considered after the
userspace pipeline is proven, and it must not block v1.

---

## 7. Permissions checklist

- **macOS:** Camera + Microphone permissions granted to `linkerd`.
- **Ubuntu:** `v4l2loopback` installed and loaded; daemon can write the loopback
  `/dev/videoN` (udev rule / `video` group); PipeWire running in the user
  session; daemon can create a virtual node.

---

## 8. Failure handling

- Camera/mic busy on the Mac → report to sink; the virtual device shows a "no
  signal" placeholder frame rather than freezing apps.
- Link drop mid-call → virtual devices emit placeholder/silence and the pipeline
  resumes automatically on reconnect (ties into core auto-reconnect).
- v4l2loopback not installed → feature reports unsupported with a clear setup
  message; the rest of Linker keeps working.

---

## 9. Open questions

- Datagrams vs. reliable streams for video on the user's actual Wi-Fi — measure.
- H.264 vs. HEVC vs. AV1 trade-off (compatibility of VAAPI decode on the Ubuntu
  GPU vs. bitrate savings).
- Direction: v1 is Mac→Ubuntu only. Is Ubuntu→Mac camera ever wanted? (Would
  require a macOS virtual camera via a CoreMediaIO/System Extension — a much
  bigger lift; out of scope for now.)
- Whether to expose multiple virtual cameras (e.g. raw vs. processed) or just one.
- GPU decode availability on the specific Ubuntu machine (VAAPI driver).