# Linker — Feature Design: Audio (Microphone + Speaker)

**Status:** implemented · **Depends on:** Core (F0) · **Reference:** the Go
`audiobridge` project (its low-latency design is ported faithfully)

Two independent audio flows ride the existing encrypted QUIC link next to the
camera:

| Flow | Direction | Mac side | Ubuntu side |
|---|---|---|---|
| **A — Microphone** | Mac → Ubuntu | default input (mic) via CoreAudio (`cpal`) | PipeWire virtual **source** "Linker Microphone" — any app picks it as its mic |
| **B — Speaker** | Ubuntu → Mac | default output via CoreAudio (`cpal`), low-latency callback | PipeWire virtual **sink** "Linker Speaker" — any app picks it as its output; only that audio goes to the Mac |

Zoom on Ubuntu: Camera = "Linker Camera", Microphone = "Linker Microphone",
Speaker = "Linker Speaker".

```
FLOW A  Mac mic ─cpal─► 48k stereo i16 ─► Framer(5ms) ─► Opus LOWDELAY ─► Packer(+2 prev) ─► QUIC datagram ─┐
                                                                                                          │
        Ubuntu "Linker Microphone" ◄─pw process()─ JitterBuffer.pull ◄─ push ◄─ decode/PLC ◄─ Reorder ◄──────┘

FLOW B  Ubuntu "Linker Speaker" ─pw process()─► Framer ─► Opus ─► Packer ─► QUIC datagram ─┐
        Mac default output ◄─cpal callback (128 frames)─ JitterBuffer.pull ◄─ … ◄─ Reorder ◄─┘
```

## 1. What is ported from audiobridge (crate `linker-audio`)

| audiobridge (Go) | Rust (`crates/linker-audio/src`) | Notes |
|---|---|---|
| `internal/codec/codec.go` | `codec.rs` | libopus via `opus` 0.4 (vendored libopus), `RESTRICTED_LOWDELAY` (CELT only), 48 kHz stereo, 2.5/5/10/20 ms, `decode(None)` = PLC with the last frame length |
| `internal/codec/frame.go` | `frame.rs` | `Resampler` (linear, stereo i16 → 48 kHz), `Framer` |
| `internal/proto/proto.go` | `proto.rs` | v3 packet **byte-for-byte**: `'A' 'B' 3 N seq(u32 LE)` then N × `[u16 LE len][opus]`, newest first; `Packer` with redundancy |
| `cmd/receiver/reorder.go` | `reorder.rs` | 64 slots, 15 ms max wait, 3 held frames, then PLC |
| `cmd/receiver/jitter.go` | `jitter.rs` | adaptive target from the largest inter-packet gap in 30 one-second buckets, quiet-region skipping, ≤ +0.5 % / −0.3 % speed, fade-out/in, crossfade trim of > 250 ms bursts |
| `cmd/receiver/main.go` receive loop | `receiver.rs` | parse → reorder → decode/PLC → jitter push; 5 ms flush tick |
| `cmd/sender/main.go` send path | `sender.rs` | Framer → encode → pack; peak meter |
| `cmd/testtone` impairments | `sim.rs` | loss / reorder / jitter / stall impairment, deterministic RNG |
| `*_test.go` | `#[cfg(test)]` in each module + `tests/` | ported 1:1, plus new simulation tests |

Differences from the reference, all deliberate:

* **Transport**: QUIC datagrams on the existing link instead of plaintext UDP
  → encrypted + authenticated (TLS 1.3, pinned peer) for free; no discovery
  needed. The Packer's maximum size is clamped to the link's
  `max_datagram_size()` (QUIC datagrams are ~1.2 KB on a fresh path).
* **Envelope** (Linker-specific, in front of the unchanged v3 packet):

  ```
  0      1                 9
  +------+-----------------+---------------------------
  | tag  | capture_ts_us   | audiobridge v3 packet …
  | u8   | u64 LE          |
  +------+-----------------+---------------------------
  tag 0xA1 = mic stream (Mac→Ubuntu), 0xA2 = speaker stream (Ubuntu→Mac)
  ```

  The first byte demultiplexes datagrams on the link: camera fragments start
  with `0xC1`, audio with `0xA1`/`0xA2`, so audio and video never share a
  queue (no head-of-line blocking between them). `capture_ts_us` (wall clock
  of the newest frame's capture, corrected on the receiver with the heartbeat
  clock-offset estimate) gives a **measured** capture→playout latency.
* Time is `std::time::Instant`; the jitter buffer's 30 s gap buckets are
  seconds since the buffer was created (Go used `time.Unix()`).
* **One algorithmic change** (found by the drift simulation): the jitter
  buffer slows playback (−0.3 % max) as soon as its smoothed level is below
  the target, instead of below target/2. With 5 ms packets and a 6 ms target
  the reference's level sawtooth touched zero before reaching target/2, so a
  sender clock only 100 ppm slower than the receiver's underran every
  ~90 s. With the change, −1000…+2000 ppm runs 3 min without an underrun
  (`linker-audio/tests/sim.rs` `drift_sweep`); all ported reference tests
  pass unchanged.
* The Go mutex inside the jitter buffer moved to the caller
  (`Arc<Mutex<JitterBuffer>>`), so the same struct is driven in virtual time
  by tests.

## 2. Control messages (new kinds, backward compatible)

Old peers decode unknown kinds as `Message::Unknown` and ignore them, so the
`Hello`/`Feature` list is **not** changed.

| kind | message | direction | meaning |
|---|---|---|---|
| 50 | `AudioStart { stream, bitrate_kbps, min_bitrate_kbps, max_bitrate_kbps, frame_ms_x10, redundancy, complexity }` | receiver → sender | "send me this stream with these parameters" (bitrate 0 = automatic within min..max) |
| 51 | `AudioStop { stream }` | receiver → sender | stop |
| 52 | `AudioStatus { stream, state, detail, sample_rate, bitrate_kbps }` | sender → receiver | Streaming / Stopped / Busy / Unavailable; re-sent when automatic bitrate changes |
| 53 | `AudioFeedback { stream, loss_permille, plc_permille, buffer_ms, target_ms, peak_gap_ms, underruns, latency_ms }` | receiver → sender, 1/s | quality report; drives automatic bitrate |

`stream` is `Mic` or `Speaker`.

* **Mic** (`mic_mode = "on_demand"` default): Ubuntu sends `AudioStart{Mic}`
  only while some app is actually reading "Linker Microphone" (the PipeWire
  node's process callback runs — its stream *state* is not reliable, see
  measurements; level meters such as pavucontrol's input tab also count as a
  reader), and `AudioStop` ~5 s after the last reader goes.
  The Mac mic (and its orange indicator) is therefore off unless e.g. Zoom
  uses it. `"always"` streams whenever the link is up.
* **Speaker**: the Mac sends `AudioStart{Speaker}` when the link comes up
  ("ready to play"). Ubuntu sends packets only while PipeWire delivers audio
  into "Linker Speaker" (an app is playing to it). The Mac opens its output
  device on the first packet and closes it after 10 s without packets.

## 3. Adaptive quality

* Default profile: Opus `RESTRICTED_LOWDELAY`, **5 ms** frames, redundancy
  **2** (each datagram carries the two previous frames → 2 consecutive lost
  datagrams are recovered without retransmission), complexity 8,
  min buffer 6 ms, max buffer 200 ms — the audiobridge defaults.
* Bitrate `auto` (`bitrate_kbps = 0`): start at 128 kbit/s; after 10
  consecutive clean feedback reports (no loss, no PLC, no underrun) step up by
  32 kbit/s to `max_bitrate_kbps` (256); on loss > 2 % or underruns, step
  down ×0.75 to `min_bitrate_kbps` (64). A fixed `bitrate_kbps` disables this.
* The jitter buffer absorbs network variance exactly as in audiobridge; its
  ±speed adjustment also absorbs the Mac↔Ubuntu clock drift.

## 4. Platform layer

`linker-platform` gains four audio capabilities (default: `Unsupported`):

```rust
fn audio_capture(&self, spec: AudioSpec, sink: PcmSink, ev: mpsc::Sender<AudioEvent>) -> Cap<Box<dyn AudioStream>>;
fn audio_playback(&self, spec: AudioSpec, src: PcmSource, ev: mpsc::Sender<AudioEvent>) -> Cap<Box<dyn AudioStream>>;
fn virtual_mic(&self, spec: AudioSpec, src: PcmSource, ev: mpsc::Sender<AudioEvent>) -> Cap<Box<dyn AudioStream>>;
fn virtual_speaker(&self, spec: AudioSpec, sink: PcmSink, ev: mpsc::Sender<AudioEvent>) -> Cap<Box<dyn AudioStream>>;
```

All PCM crossing the trait is **48 kHz interleaved stereo i16**; backends
convert (channel up/down-mix, `Resampler`). `PcmSource` is the jitter buffer
pull (called from the real-time callback), `PcmSink` receives captured PCM.

| | macOS (`cpal` / CoreAudio) | Ubuntu (`pipewire` 0.10, in-process) |
|---|---|---|
| `audio_capture` | default (or named) input device, device-default IO buffer | stream with optional `target.object` (used by `check`/tests) |
| `audio_playback` | default (or named) output, buffer 128 frames (≈2.67 ms) — the `LowLatency`/`NoFixedSizedCallback` equivalent | stream with optional `target.object` |
| `virtual_mic` | — | `pw_stream` OUTPUT, `media.class = Audio/Source`, `node.name = linker_microphone`, `node.description = "Linker Microphone"` |
| `virtual_speaker` | — | `pw_stream` INPUT, `media.class = Audio/Sink`, `node.name = linker_speaker`, `node.description = "Linker Speaker"` |

macOS notes (verify on hardware): devices open at their nominal rate and
default channel count; cpal only reports F32/I16 virtual formats, so any
other one is opened as F32 (the AudioUnit converts). When cpal builds a
stream it also sets the device's **physical** format to one matching the
client format (e.g. a 24-bit integer interface can be switched to 32-bit
float, which affects other apps); the backend snapshots the physical format
before the first Linker stream on a device and restores it when the last one
closes, unless another app changed it meanwhile. The fixed 128-frame buffer
(`kAudioDevicePropertyBufferFrameSize`) is requested for playback only.

Ubuntu needs PipeWire ≥ 0.3.49 (`pw_buffer.requested`, Ubuntu ≥ 23.04; the
default sound server since 22.10). Streams use `RT_PROCESS` (callback on
PipeWire's real-time data thread) and `node.latency = 128/48000`.

The virtual nodes are owned by `linkerd` (they disappear when it exits — no
stale devices), are visible to PulseAudio clients through `pipewire-pulse`
(Zoom, Chrome, Firefox) and need **no kernel module**. A virtual sink node
receives exactly the audio routed to it, which is what capturing a null-sink's
monitor would give, minus one graph hop.

## 5. Failure handling

* **Link drop / sleep / wake**: the receiver resets reorder + decoder state and
  the jitter buffer fades to silence; the virtual mic keeps producing silence
  so apps don't error. On reconnect the sink re-requests the streams; the
  first packet of a new session is detected by the reorder `restart` path.
* **Mac device change / CoreAudio error after wake**: the backend reports an
  error event; the feature reopens the device with back-off. Default-device
  changes (headphones plugged) are polled every 2 s and followed.
* **PipeWire restart**: the stream's error/unconnected state is reported and
  the feature recreates the node with back-off.
* **Mic permission denied**: `AudioStatus{Unavailable}` → Ubuntu status shows
  the reason; the virtual mic stays (silent).

## 6. Observability

`linkerd status` prints one line per active stream, modelled on
audiobridge's stats line:

```
audio mic     : recv playing | buf  10ms (target   6) | gap   6ms | recovered 0 PLC 0 late 0 | underruns 0 trims 0 | speed +0.0% | loss 0.0% | 256 kbps 5 ms x3 | latency 17 ms | cb 3.0ms | Linker Microphone
audio speaker : send streaming | 200 pkt/s 810 kbps | peak -3.5 dBFS | peer buf 10ms (target 6) PLC 0 underruns 0 | 256 kbps 5 ms x3 | latency 16 ms | Linker Speaker
```

`x3` = copies of each frame on the wire (the frame + `redundancy` = 2 previous).

`linkerd check` probes each machine's audio path (see runbooks).

## 7. Testing

* Ported unit tests: reorder (clean / reorder / 5 % / 10 % + reorder),
  parse rejects v2, packer fits MTU, jitter (steady state, adapts to stalls,
  smooth trim, quiet skip), codec round trip SNR + speed (10/5/2.5 ms), PLC,
  resampler 44.1→48 kHz, framer.
* Simulation (`linker-audio/tests/sim.rs`): the full sender → impaired
  network → receiver → jitter chain with real Opus in virtual time, for
  jitter / loss / reorder / stall modes, asserting recovery, no underruns
  after adaptation, no clicks and continuous phase.
* Integration (`linker-core/tests/audio.rs`): two real daemons over QUIC with
  simulated real-time devices: both flows + camera together, on-demand
  start/stop, link drop + reconnect recovery, independent directions.
* PipeWire (`linker-linux/tests/pipewire_audio.rs`, `--ignored`, needs a
  running PipeWire): virtual nodes appear, idle/active demand detection, tone
  through "Linker Speaker" → engine (lossy NetSim) → "Linker Microphone" with
  latency + gap measurement, nodes removed on drop.
* Real two-daemon end to end (`runbook/audio-e2e.sh`): two `linkerd`
  processes over QUIC, PipeWire on both ends, a Zoom-like app plays into
  "Linker Speaker" and records "Linker Microphone" (both flows in series);
  scenarios `steady`, `sleepwake` (SIGSTOP > heartbeat timeout), `restart`
  (kill -9), `impair`.
* Results: [runbook/measurements.md](../runbook/measurements.md#audio).
