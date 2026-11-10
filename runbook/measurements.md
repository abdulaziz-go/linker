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

## Audio

Microphone (Mac → Ubuntu "Linker Microphone") and speaker (Ubuntu "Linker
Speaker" → Mac) streams: Opus `RESTRICTED_LOWDELAY`, 48 kHz stereo, 5 ms
frames, 2 redundant frames per packet, adaptive jitter buffer 6–200 ms
(audiobridge defaults). Measured on the Ubuntu machine above; the Mac side
was type-checked only — its device latencies are the one part still to be
measured (procedure below).

### Engine (deterministic simulation, `crates/linker-audio/tests/sim.rs`)

Virtual time, real Opus, 128-frame playback clock; "gap" = ≥ 2 ms of digital
silence or a 10 ms window below 30 % RMS after warm-up.

| Network | Result |
|---|---|
| clean | 0 gaps, 0 PLC, buffer 4 ms (min 6 ms target) |
| jitter 0–8 ms | 0 gaps, target 26 ms |
| 5 % random loss | 0 gaps, 1006 frames rebuilt from redundancy, ≤ 1 PLC / 6000 frames |
| 10 / 15 / 20 % loss (5 seeds × 50 s each) | 0 underruns; PLC 56 / 189 / 447 frames (concealed, not silent) |
| 25 % loss | 2 underruns in 250 s — the limit of the design |
| 20 % reordering | 0 gaps, 0 PLC |
| 100 ms stall every 1 s (AWDL-like) | 0 gaps after adaptation, target 105 ms |
| 5 % loss + 10 % reorder + 5 ms jitter + 100 ms stall / 1.5 s | 0 gaps, ≤ 1 PLC |
| clock drift −1000 … +2000 ppm, 3 min each | 0 underruns (real clocks differ by 10–100 ppm) |

The drift sweep found one weakness in the reference algorithm (it underran
every ~90 s at −100 ppm); see the DEVIATION note in `jitter.rs`.

### Two daemons over QUIC, simulated real-time devices (`crates/linker-core/tests/audio.rs`)

Both flows **and** the camera (30 fps) at once, producer clocks +200 / −150 ppm:

| Run | Mic | Speaker | Camera | A/V skew |
|---|---|---|---|---|
| 300 s (`LINKER_SOAK_SECS=300`) | 0 dropouts, 26.3 ms | 0 dropouts, 19.4 ms | 9091 frames (30.3 fps), 21.8 ms | +4.5 ms |

Latency = capture callback → playout callback (markers), i.e. glass-to-glass
minus the real devices' own buffers. A/V skew = mic latency − camera pipeline
latency; lip-sync is undetectable within −45 … +125 ms (ITU-R BT.1359).
Impaired (5 % loss, 10 % reorder, 4 ms jitter, 60 ms stall every 2 s on both
directions): 0 dropouts after adaptation, ~200 frames rebuilt per 20 s, 1 PLC,
buffer 61–65 ms, latency ~80 ms.

### Real PipeWire, two real `linkerd` processes (`runbook/audio-e2e.sh`)

A Zoom-like app plays a tone into "Linker Speaker" and records "Linker
Microphone"; the "Mac" daemon plays into a null sink and captures its
monitor, so the sound crosses **both** flows over the real encrypted link
with PipeWire at every end (128-frame / 2.67 ms quantum).

| Scenario | Round-trip (both flows) | Result |
|---|---|---|
| steady 240 s | median 27.5 ms, p95 46 ms | 1 gap (22 ms, at start-up) |
| PipeWire graph reconfigurations every 20 s, 150 s | median 35 ms, p95 36 ms | 0 gaps, 0 underruns |
| sleep/wake: "Mac" frozen 20 s (> 15 s heartbeat) | — | link re-established, audio back **0.7 s after wake**, both directions |
| `kill -9` + restart after 3 s | — | audio back when the link is (15 s: dead-peer detection of the core, pre-existing) |
| 5 % loss, 10 % reorder, 5 ms jitter, 80 ms stall / 1.5 s | median 199 ms | 0 gaps after ~5 s adaptation; ~680 rebuilt, 2 PLC per direction; bitrate auto-lowered to 64 kbps |

Per direction that is ≈ 14–20 ms on a clean link (`linkerd status` latency
column), of which 6 ms is the minimum jitter buffer, 2.5 ms Opus look-ahead,
5 ms frame and ~2.7 ms per PipeWire/CoreAudio period. `linkerd check` on
Ubuntu (app → Linker Speaker → engine → Linker Microphone → app, no network):
**11.8 ms**.

A long real-PipeWire soak found a PipeWire quirk: on a graph
reconfiguration the virtual mic's stream *state* stayed "Paused" for 15 s
while its process callback (and the recording app) kept running; Linker read
that as "no app records" and stopped the Mac mic → a 210 ms gap and a burst.
Demand detection now uses the process callback only.

### Estimated Mac ↔ Ubuntu budget (one direction, good 5 GHz Wi-Fi)

| Stage | ms |
|---|---|
| capture device buffer (Mac mic / PipeWire) | 3–10 |
| frame accumulation (5 ms frame) + Opus look-ahead | 5 + 2.5 |
| Wi-Fi | 2–10 |
| adaptive jitter buffer (follows the worst gap of the last 30 s) | 6–30 |
| playout period (128 frames) + device latency | 3–12 |
| **total** | **≈ 20–60 ms** (audiobridge measured 20–30 ms on a 5 GHz hotspot) |

### How to measure on the real machines

1. `linkerd status` on both: `latency` per stream (network part measured
   with the heartbeat clock offset), `buf/target`, `gap`, `PLC`, `underruns`.
2. `linkerd check` on the Mac: mic + output device period and device latency.
3. Glass-to-glass (mic): in Zoom's *Test Mic* or `pw-record --target
   linker_microphone /tmp/m.wav` on Ubuntu while tapping the Mac's mic next to
   Ubuntu's own mic recording into a second track (Audacity: record Ubuntu's
   built-in mic + Linker Microphone together); the offset between the two taps
   is the mic latency. Speaker: play a click to "Linker Speaker" and record it
   with Ubuntu's built-in mic next to the Mac's speakers together with the
   click's own loopback.
