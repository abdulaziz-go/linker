//! `linkerd check` — audio probes.
//!
//! * **Ubuntu (sink role)**: creates check-only copies of the virtual devices
//!   ("Linker Speaker (check)" / "Linker Microphone (check)", so a running
//!   daemon's devices are untouched), plays a tone into the speaker like an
//!   app would, runs it through the real engine (Opus LOWDELAY → redundancy
//!   packets → reorder → decode → adaptive jitter buffer) and records it back
//!   from the microphone like Zoom would. Reports sample rate, callback
//!   period, measured latency (tone markers) and loss / gaps.
//! * **Mac (source role)**: opens the microphone (rate, channels, period, level
//!   — a silent mic usually means the permission is missing) and the output
//!   (rate, period, device latency, playing silence so nothing is heard),
//!   then measures the engine's own latency in-process.
//! * Both: if the daemon is running, its live audio stats.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linker_audio::frame::{dbfs, peak};
use linker_audio::jitter::JitterBuffer;
use linker_audio::proto;
use linker_audio::receiver::Receiver;
use linker_audio::sender::{Sender, SenderParams};
use linker_core::config::AudioConfig;
use linker_platform::{AudioSpec, AudioStream, PlatformBackend};

const MARKER_EVERY: u64 = 24_000; // 0.5 s
const MARKER_LEN: u64 = 720; // 15 ms
const MARKER_THRESHOLD: u16 = 15_000;

fn ok(name: &str, msg: String) {
    println!("  [ OK ] {name:<22} {msg}");
}
fn bad(name: &str, msg: String) {
    println!("  [FAIL] {name:<22} {msg}");
}
fn warn(name: &str, msg: String) {
    println!("  [WARN] {name:<22} {msg}");
}

/// Tone generator with a loud marker every 0.5 s; records when each marker
/// left the generator (ns since `t0`).
struct Tone {
    pos: u64,
    phase: f64,
    t0: Instant,
    markers: Arc<Mutex<VecDeque<u64>>>,
}

impl Tone {
    fn fill(&mut self, out: &mut [i16]) {
        let now = (Instant::now() - self.t0).as_nanos() as u64;
        let n = out.len() / 2;
        for i in 0..n {
            let p = (self.pos + i as u64) % MARKER_EVERY;
            if p == 0 {
                self.markers
                    .lock()
                    .unwrap()
                    .push_back(now + i as u64 * 1_000_000_000 / 48_000);
            }
            let amp = if p < MARKER_LEN { 22_000.0 } else { 5_000.0 };
            let v = (amp * self.phase.sin()) as i16;
            self.phase += 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
            if self.phase > 2.0 * std::f64::consts::PI {
                self.phase -= 2.0 * std::f64::consts::PI;
            }
            out[2 * i] = v;
            out[2 * i + 1] = v;
        }
        self.pos += n as u64;
    }
}

/// Detects markers (latency) and digital-silence gaps in received audio.
#[derive(Default)]
struct Meter {
    started: bool,
    samples: u64,
    zero_run: u64,
    gaps: u32,
    lat_ms: Vec<f64>,
    last_detect: Option<Instant>,
}

impl Meter {
    fn feed(&mut self, pcm: &[i16], t0: Instant, markers: &Mutex<VecDeque<u64>>) {
        let now = Instant::now();
        let n = pcm.len() / 2;
        for i in 0..n {
            let v = pcm[2 * i];
            self.samples += 1;
            if v != 0 {
                if self.zero_run >= 96 {
                    self.gaps += 1;
                }
                self.started = true;
                self.zero_run = 0;
            } else if self.started {
                self.zero_run += 1;
            }
            if v.unsigned_abs() > MARKER_THRESHOLD
                && self
                    .last_detect
                    .is_none_or(|t| now - t > Duration::from_millis(250))
            {
                self.last_detect = Some(now);
                let heard = ((now - t0).as_nanos() as u64)
                    .saturating_sub((n - i) as u64 * 1_000_000_000 / 48_000);
                let mut mk = markers.lock().unwrap();
                while mk.len() > 1 && mk[1] <= heard {
                    mk.pop_front();
                }
                if let Some(&t) = mk.front() {
                    if t <= heard {
                        self.lat_ms.push((heard - t) as f64 / 1e6);
                    }
                    mk.pop_front();
                }
            }
        }
    }

    fn median_ms(&self) -> Option<f64> {
        let mut v = self.lat_ms.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v.get(v.len() / 2).copied()
    }
}

/// The engine between a PCM producer and a consumer, run in-process with a
/// 1 ms network/flush thread (no network loss — the real link's loss is
/// shown by `linkerd status`).
struct Engine {
    jb: Arc<Mutex<JitterBuffer>>,
    rx: Arc<Mutex<Receiver>>,
    tx: Arc<Mutex<Sender>>,
    inbox: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    fn new(cfg: &AudioConfig) -> anyhow::Result<Engine> {
        let jb = Arc::new(Mutex::new(JitterBuffer::new(
            48_000,
            2,
            cfg.min_buffer_ms,
            cfg.max_buffer_ms,
        )));
        let rx = Arc::new(Mutex::new(Receiver::new(jb.clone())?));
        let tx = Arc::new(Mutex::new(Sender::new(
            proto::TAG_MIC,
            SenderParams {
                bitrate_bps: if cfg.bitrate_kbps == 0 {
                    128_000
                } else {
                    cfg.bitrate_kbps as i32 * 1000
                },
                complexity: cfg.complexity as i32,
                frame_ms: cfg.frame_ms,
                redundancy: cfg.redundancy as usize,
                ..Default::default()
            },
        )?));
        let inbox: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (rx, inbox, stop) = (rx.clone(), inbox.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let pkts = std::mem::take(&mut *inbox.lock().unwrap());
                    let now = Instant::now();
                    let mut r = rx.lock().unwrap();
                    for p in pkts {
                        if let Some((_, _, pk)) = proto::unwrap(&p) {
                            let _ = r.on_packet(pk, now);
                        }
                    }
                    r.on_tick(now);
                    drop(r);
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };
        Ok(Engine {
            jb,
            rx,
            tx,
            inbox,
            stop,
            thread: Some(thread),
        })
    }

    fn sink(&self) -> linker_platform::PcmSink {
        let (tx, inbox) = (self.tx.clone(), self.inbox.clone());
        Box::new(move |pcm: &[i16]| {
            let mut out = Vec::new();
            tx.lock().unwrap().push(pcm, 0, |d| out.push(d.to_vec()));
            inbox.lock().unwrap().extend(out);
        })
    }

    fn source(&self) -> linker_platform::PcmSource {
        let jb = self.jb.clone();
        Box::new(move |out: &mut [i16]| jb.lock().unwrap().pull(out, Instant::now()))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn period_str(s: &dyn AudioStream) -> String {
    let i = s.info();
    if i.period_frames > 0 && i.sample_rate > 0 {
        format!(
            "period {} frames ({:.2} ms)",
            i.period_frames,
            i.period_frames as f64 * 1000.0 / i.sample_rate as f64
        )
    } else {
        "period ?".into()
    }
}

/// Ubuntu: virtual sink + source through the engine, with apps simulated by
/// a playback stream and a capture stream.
pub fn check_virtual(b: &dyn PlatformBackend, cfg: &AudioConfig) {
    let secs = 4;
    let engine = match Engine::new(cfg) {
        Ok(e) => e,
        Err(e) => return bad("audio engine", format!("{e:#}")),
    };
    let (ev, _ev_rx) = tokio::sync::mpsc::channel(8);
    let spk_name = "linker_check_speaker";
    let mic_name = "linker_check_microphone";
    let speaker = match b.virtual_speaker(
        AudioSpec {
            device: Some(spk_name.into()),
            description: format!("{} (check)", cfg.speaker_name),
            period_ms: cfg.period_ms,
        },
        engine.sink(),
        ev.clone(),
    ) {
        Ok(s) => s,
        Err(e) => return bad("audio virtual speaker", e.0),
    };
    let mic = match b.virtual_mic(
        AudioSpec {
            device: Some(mic_name.into()),
            description: format!("{} (check)", cfg.mic_name),
            period_ms: cfg.period_ms,
        },
        engine.source(),
        ev.clone(),
    ) {
        Ok(s) => s,
        Err(e) => return bad("audio virtual mic", e.0),
    };

    let t0 = Instant::now();
    let markers: Arc<Mutex<VecDeque<u64>>> = Arc::default();
    let meter = Arc::new(Mutex::new(Meter::default()));
    let rec = {
        let (meter, markers) = (meter.clone(), markers.clone());
        b.audio_capture(
            AudioSpec {
                device: Some(mic_name.into()),
                description: String::new(),
                period_ms: cfg.period_ms,
            },
            Box::new(move |pcm: &[i16]| meter.lock().unwrap().feed(pcm, t0, &markers)),
            ev.clone(),
        )
    };
    let rec = match rec {
        Ok(r) => r,
        Err(e) => {
            return bad(
                "audio virtual mic",
                format!("an app cannot record from it: {}", e.0),
            )
        }
    };
    let mut tone = Tone {
        pos: 0,
        phase: 0.0,
        t0,
        markers: markers.clone(),
    };
    let play = match b.audio_playback(
        AudioSpec {
            device: Some(spk_name.into()),
            description: String::new(),
            period_ms: cfg.period_ms,
        },
        Box::new(move |out: &mut [i16]| tone.fill(out)),
        ev.clone(),
    ) {
        Ok(p) => p,
        Err(e) => {
            return bad(
                "audio virtual speaker",
                format!("an app cannot play to it: {}", e.0),
            )
        }
    };
    std::thread::sleep(Duration::from_millis(1500)); // warm-up / adaptation
    let under0 = engine.jb.lock().unwrap().stats().underruns;
    *meter.lock().unwrap() = Meter {
        started: true,
        ..Default::default()
    };
    std::thread::sleep(Duration::from_secs(secs));
    let m = std::mem::take(&mut *meter.lock().unwrap());
    let js = engine.jb.lock().unwrap().stats();
    let rs = engine.rx.lock().unwrap().stats();
    let (spk_active, mic_active) = (speaker.active(), mic.active());
    let (spk_info, mic_info) = (speaker.info(), mic.info());
    let (spk_p, mic_p) = (period_str(&*speaker), period_str(&*mic));
    drop(play);
    drop(rec);

    if spk_active && m.samples > 0 {
        ok(
            "audio virtual speaker",
            format!(
                "\"{}\" receives app audio — {} Hz, {}",
                spk_info.device, spk_info.sample_rate, spk_p
            ),
        );
    } else {
        bad(
            "audio virtual speaker",
            "no audio arrived from a playing app".into(),
        );
    }
    if mic_active && m.samples > 0 {
        ok(
            "audio virtual mic",
            format!(
                "\"{}\" delivers to recording apps — {} Hz, {}",
                mic_info.device, mic_info.sample_rate, mic_p
            ),
        );
    } else {
        bad(
            "audio virtual mic",
            "a recording app received nothing".into(),
        );
    }
    let recorded = m.samples as f64 / 48_000.0;
    let under = js.underruns - under0;
    let msg = format!(
        "speaker → Opus → jitter buffer → mic: latency {} | {:.1}s recorded, gaps {}, underruns {}, PLC {} of {} frames | buffer {} ms",
        m.median_ms()
            .map_or("? (no markers)".to_string(), |l| format!("{l:.1} ms")),
        recorded,
        m.gaps,
        under,
        rs.plc,
        rs.frames,
        js.avg_ms
    );
    if m.gaps == 0 && under == 0 && recorded >= secs as f64 * 0.9 && m.median_ms().is_some() {
        ok("audio loop (local)", msg);
    } else {
        bad("audio loop (local)", msg);
    }
}

/// Mac: the real microphone and output, then the engine's own latency.
pub fn check_devices(b: &dyn PlatformBackend, cfg: &AudioConfig) {
    let (ev, mut ev_rx) = tokio::sync::mpsc::channel(8);
    // Microphone.
    let level = Arc::new(AtomicU64::new(0));
    let frames = Arc::new(AtomicU64::new(0));
    let cap = {
        let (level, frames) = (level.clone(), frames.clone());
        b.audio_capture(
            AudioSpec {
                device: cfg.input_device.clone(),
                description: String::new(),
                period_ms: cfg.period_ms,
            },
            Box::new(move |pcm: &[i16]| {
                level.fetch_max(peak(pcm) as u64, Ordering::Relaxed);
                frames.fetch_add(pcm.len() as u64 / 2, Ordering::Relaxed);
            }),
            ev.clone(),
        )
    };
    match cap {
        Ok(c) => {
            std::thread::sleep(Duration::from_secs(2));
            let i = c.info();
            let gap = c.take_max_callback_gap_ms();
            drop(c);
            let got = frames.load(Ordering::Relaxed);
            let pk = level.load(Ordering::Relaxed) as u16;
            let msg = format!(
                "\"{}\" {} Hz {} ch, {}, max callback gap {gap:.1} ms, level {} ({:.1}s captured)",
                i.device,
                i.sample_rate,
                i.channels,
                period_str_info(&i),
                dbfs(pk),
                got as f64 / 48_000.0
            );
            if got < 48_000 {
                bad("audio mic capture", format!("{msg} — callbacks stalled"));
            } else if pk == 0 {
                warn(
                    "audio mic capture",
                    format!(
                        "{msg} — digital silence: microphone permission denied or input muted?"
                    ),
                );
            } else {
                ok("audio mic capture", msg);
            }
        }
        Err(e) => bad("audio mic capture", e.0),
    }
    // Output (plays silence: nothing is heard).
    let pulled = Arc::new(AtomicU64::new(0));
    let out = {
        let pulled = pulled.clone();
        b.audio_playback(
            AudioSpec {
                device: cfg.output_device.clone(),
                description: String::new(),
                period_ms: cfg.period_ms,
            },
            Box::new(move |out: &mut [i16]| {
                out.fill(0);
                pulled.fetch_add(out.len() as u64 / 2, Ordering::Relaxed);
            }),
            ev.clone(),
        )
    };
    match out {
        Ok(o) => {
            std::thread::sleep(Duration::from_millis(1500));
            let i = o.info();
            let gap = o.take_max_callback_gap_ms();
            drop(o);
            let got = pulled.load(Ordering::Relaxed);
            let msg = format!(
                "\"{}\" {} Hz {} ch, {}, device latency {:.1} ms, max callback gap {gap:.1} ms",
                i.device,
                i.sample_rate,
                i.channels,
                period_str_info(&i),
                i.device_latency_ms
            );
            if got >= 48_000 {
                ok("audio output", msg);
            } else {
                bad("audio output", format!("{msg} — callbacks stalled"));
            }
        }
        Err(e) => bad("audio output", e.0),
    }
    while let Ok(linker_platform::AudioEvent::Failed(e)) = ev_rx.try_recv() {
        warn("audio device event", e);
    }
    engine_latency(cfg);
}

fn period_str_info(i: &linker_platform::AudioStreamInfo) -> String {
    if i.period_frames > 0 && i.sample_rate > 0 {
        format!(
            "period {} frames ({:.2} ms)",
            i.period_frames,
            i.period_frames as f64 * 1000.0 / i.sample_rate as f64
        )
    } else {
        "period default".into()
    }
}

/// In-process engine latency: tone → Sender → Receiver → jitter buffer
/// pulled on a 128-frame clock (no devices, no network).
fn engine_latency(cfg: &AudioConfig) {
    let engine = match Engine::new(cfg) {
        Ok(e) => e,
        Err(e) => return bad("audio engine", format!("{e:#}")),
    };
    let t0 = Instant::now();
    let markers: Arc<Mutex<VecDeque<u64>>> = Arc::default();
    let mut tone = Tone {
        pos: 0,
        phase: 0.0,
        t0,
        markers: markers.clone(),
    };
    let mut sink = engine.sink();
    let mut src = engine.source();
    let mut meter = Meter::default();
    let mut buf_in = vec![0i16; 128 * 2];
    let mut buf_out = vec![0i16; 128 * 2];
    let period = Duration::from_secs_f64(128.0 / 48_000.0);
    let start = Instant::now();
    let mut n = 0u32;
    while start.elapsed() < Duration::from_secs(3) {
        let due = start + period * n;
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        tone.fill(&mut buf_in);
        sink(&buf_in);
        src(&mut buf_out);
        if start.elapsed() > Duration::from_secs(1) {
            meter.feed(&buf_out, t0, &markers);
        }
        n += 1;
    }
    let js = engine.jb.lock().unwrap().stats();
    match meter.median_ms() {
        Some(l) => ok(
            "audio engine",
            format!(
                "Opus LOWDELAY {} ms frames ×{} redundancy, buffer {}–{} ms: engine latency {l:.1} ms (excl. devices + network), gaps {}",
                cfg.frame_ms, cfg.redundancy, cfg.min_buffer_ms, cfg.max_buffer_ms, meter.gaps
            ),
        ),
        None => bad("audio engine", format!("no audio came through ({js:?})")),
    }
}

/// Live stats from the running daemon (if any).
pub fn live(status: &linker_core::status::Status) {
    if status.audio.is_empty() {
        println!(
            "  [ -- ] {:<22} no audio streams yet (feature: {})",
            "audio live",
            status
                .features
                .get("audio")
                .map(String::as_str)
                .unwrap_or("not reported")
        );
        return;
    }
    for a in &status.audio {
        let line = linker_core::status::render_audio(a);
        let healthy = a.underruns == 0 && a.loss_permille < 50;
        if healthy {
            ok("audio live", line);
        } else {
            warn("audio live", line);
        }
    }
}
