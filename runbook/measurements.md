# Measurements

What was measured during development, how, and what still needs real hardware.

## Environment

* Ubuntu side: Intel i5-11400F (12 threads), Ubuntu 24.04-class kernel 7.0, X11/GNOME.
* macOS side: **not available during development** — the macOS backend was
  type-checked (`cargo check/clippy --target aarch64-apple-darwin`) but not run.
  Numbers involving the Mac are therefore *estimates* below and must be
  measured on the real pair (procedure included).

## Link layer (measured, two real `linkerd` processes on one host, LAN IP)

| Metric | Result |
|---|---|
| mDNS discovery → connected (fresh start, both up) | ~2 s |
| QUIC RTT reported by heartbeat | 0.5–0.9 ms (same host) |
| Peer stopped gracefully (SIGTERM) → other side notices | immediate (QUIC CONNECTION_CLOSE) |
| Peer vanishes without closing → detected | ≤ 15 s (3 missed 5 s heartbeats / QUIC idle timeout) |
| Restarted peer → link restored | ~0.5 s |
| "Peer not found" notifications per outage | exactly 1 (integration test `reconnects_after_peer_restart_and_notifies_once`) |

## Camera pipeline

### Ubuntu decode cost (measured)

`cargo test --release -p linker-linux bench_decode_720p -- --ignored --nocapture`

| Content | Avg frame size | Decode + I420 pack | CPU at 30 fps |
|---|---|---|---|
| 720p moving texture (camera-like) | 14 KiB | **3.98 ms/frame** | ~12 % of one core |
| 720p smooth gradient | 1 KiB | 2.22 ms/frame | ~7 % of one core |

Decoding is software (OpenH264). VAAPI hardware decode is not implemented in
this version; at these costs it isn't the bottleneck, but it is the obvious
next optimisation for lower CPU.

### Transport (measured in integration tests)

A 50 KB keyframe is fragmented into ~40 QUIC datagrams and reassembled intact;
11 frames end-to-end through the real QUIC link
(`camera_on_demand_stream`). Lost fragments drop the frame and trigger a
keyframe request (unit tests in `linker-proto::media`).

### Glass-to-glass latency (estimate — measure on hardware)

| Stage | Estimate |
|---|---|
| Sensor exposure + AVFoundation delivery | 10–25 ms |
| VideoToolbox H.264 encode (hardware, real-time, Baseline, no B-frames) | 3–8 ms |
| Wi-Fi transfer of a 15–60 KB frame | 2–10 ms |
| Reassembly + jitter buffer (`jitter_ms = 20`) | 20 ms |
| OpenH264 decode + v4l2loopback write | ~4 ms |
| Consuming app render (browser/Zoom) | 8–33 ms |
| **Total** | **≈ 50–100 ms** (target ≤ 80 ms) |

`jitter_ms` can be lowered to 0–10 on a clean LAN to cut ~10–20 ms.

### How to measure on the real machines

1. **Pipeline latency (built in)**: while an app uses "Linker Camera",
   `linkerd status` on Ubuntu shows `latency` — the median time from the Mac's
   capture callback to the frame being written to `/dev/video10`, corrected for
   the clock offset estimated from heartbeat ping/pongs. It also shows fps and loss.
2. **Glass-to-glass**: on the Mac open a millisecond stopwatch
   (e.g. <https://www.online-stopwatch.com/full-screen-stopwatch/>), point the
   Mac's camera at a phone showing the same stopwatch, show "Linker Camera"
   full-screen on Ubuntu, and photograph both screens together. The difference
   between the two clocks in the photo is glass-to-glass latency. Average 10 shots.
3. **fps**: `ffmpeg -f v4l2 -i /dev/video10 -t 10 -f null -` reports the
   delivered frame rate (expect ≈ 30).
