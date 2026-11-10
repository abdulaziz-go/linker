//! Simulated real-time audio devices for the two-daemon integration tests.
//!
//! Every device is a thread that runs on the real clock with a 128-frame
//! (2.67 ms) period, like the CoreAudio / PipeWire callbacks it stands in
//! for. Producers (Mac mic, Ubuntu "Linker Speaker") generate a 440 Hz sine
//! with a loud 30 ms **marker** once a second; consumers (Mac output, Ubuntu
//! "Linker Microphone") pull PCM and measure:
//!
//! * **dropouts** — runs of ≥ 2 ms of exact digital silence after audio started
//!   (a sine never has those; a jitter-buffer underrun produces them),
//! * **clicks** — the largest sample-to-sample jump outside markers,
//! * **latency** — marker produced (wall clock) → marker played (wall clock),
//!   i.e. glass-to-glass minus the real devices' own latency.
//!
//! Producers can run with a clock-drift offset (ppm) to prove the adaptive
//! buffer absorbs it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linker_platform::*;
use tokio::sync::mpsc;

pub const PERIOD: usize = 128;
const RATE: f64 = 48_000.0;
const BASE_AMP: f64 = 6_000.0;
const MARKER_AMP: f64 = 22_000.0;
const MARKER_LEN: usize = 1_440; // 30 ms
const MARKER_RAMP: usize = 144; // 3 ms
const MARKER_EVERY: u64 = 48_000; // 1 s
const MARKER_THRESHOLD: i16 = 15_000;
const DROPOUT_SAMPLES: u32 = 96; // 2 ms of exact zeros

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Mac microphone (producer).
    Capture = 0,
    /// Mac speakers (consumer).
    Playback = 1,
    /// Ubuntu "Linker Microphone" (consumer, only pulls while an app records).
    VirtualMic = 2,
    /// Ubuntu "Linker Speaker" (producer, only delivers while an app plays).
    VirtualSpeaker = 3,
}

impl Kind {
    fn idx(self) -> usize {
        self as usize
    }
    /// Which producer feeds this consumer through the link.
    fn producer(self) -> Kind {
        match self {
            Kind::VirtualMic => Kind::Capture,
            Kind::Playback => Kind::VirtualSpeaker,
            k => k,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct Meter {
    /// First non-zero sample seen.
    pub started: bool,
    pub samples: u64,
    pub nonzero_samples: u64,
    zero_run: u32,
    pub dropouts: u32,
    pub longest_dropout_ms: f64,
    pub max_jump: i32,
    prev: i16,
    in_marker: u32,
    pub latencies_ms: Vec<f64>,
    last_detect: Option<Instant>,
}

impl Meter {
    pub fn median_latency_ms(&self) -> Option<f64> {
        let mut v = self.latencies_ms.clone();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Some(v[v.len() / 2])
    }
}

#[derive(Default)]
pub struct AudioSim {
    /// An app is recording from the virtual mic (PipeWire schedules it).
    pub mic_consumer: AtomicBool,
    /// An app is playing into the virtual speaker.
    pub app_playing: AtomicBool,
    /// Producer clock offset in ppm (+ = faster than nominal).
    pub drift_ppm: [AtomicI64; 4],
    /// Devices currently open / ever opened, by kind.
    pub live: [AtomicU32; 4],
    pub opened: [AtomicU32; 4],
    /// Failure injector of the most recently opened device of each kind.
    fail: Mutex<[Option<mpsc::Sender<AudioEvent>>; 4]>,
    /// Wall-clock times (µs) markers were produced, per producer kind.
    /// Shared between the two nodes of a test (see [`AudioSim::sharing_markers_with`])
    /// so a consumer can find the producer's marker on the other node.
    markers: Arc<[Mutex<VecDeque<u64>>; 4]>,
    pub meters: [Mutex<Meter>; 4],
    /// Producer: frames generated.
    pub produced: [AtomicU64; 4],
}

impl AudioSim {
    /// A fresh simulator that shares `other`'s marker store.
    pub fn sharing_markers_with(other: &AudioSim) -> AudioSim {
        AudioSim {
            markers: other.markers.clone(),
            ..Default::default()
        }
    }

    pub fn live(&self, k: Kind) -> u32 {
        self.live[k.idx()].load(Ordering::SeqCst)
    }
    pub fn opened(&self, k: Kind) -> u32 {
        self.opened[k.idx()].load(Ordering::SeqCst)
    }
    pub fn meter(&self, k: Kind) -> Meter {
        self.meters[k.idx()].lock().unwrap().clone()
    }
    /// Forget the measurements so far (call after warm-up).
    pub fn reset_meter(&self, k: Kind) {
        let mut m = self.meters[k.idx()].lock().unwrap();
        let started = m.started;
        *m = Meter::default();
        m.started = started;
    }
    /// Make the device of kind `k` report a failure (device unplugged,
    /// CoreAudio error after wake, PipeWire restart…).
    pub fn inject_failure(&self, k: Kind, why: &str) -> bool {
        let tx = self.fail.lock().unwrap()[k.idx()].clone();
        tx.is_some_and(|t| t.try_send(AudioEvent::Failed(why.into())).is_ok())
    }
}

struct Dev {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    kind: Kind,
    sim: Arc<AudioSim>,
    gap_ns: Arc<AtomicU64>,
}

impl AudioStream for Dev {
    fn info(&self) -> AudioStreamInfo {
        AudioStreamInfo {
            device: format!("mock {:?}", self.kind),
            sample_rate: 48_000,
            channels: 2,
            period_frames: PERIOD as u32,
            device_latency_ms: 0.0,
        }
    }
    fn active(&self) -> bool {
        match self.kind {
            Kind::VirtualMic => self.sim.mic_consumer.load(Ordering::SeqCst),
            Kind::VirtualSpeaker => self.sim.app_playing.load(Ordering::SeqCst),
            _ => true,
        }
    }
    fn take_max_callback_gap_ms(&self) -> f64 {
        self.gap_ns.swap(0, Ordering::SeqCst) as f64 / 1e6
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.sim.live[self.kind.idx()].fetch_sub(1, Ordering::SeqCst);
    }
}

fn register(sim: &Arc<AudioSim>, kind: Kind, ev: mpsc::Sender<AudioEvent>) {
    sim.live[kind.idx()].fetch_add(1, Ordering::SeqCst);
    sim.opened[kind.idx()].fetch_add(1, Ordering::SeqCst);
    sim.fail.lock().unwrap()[kind.idx()] = Some(ev);
}

fn now_wall_us() -> u64 {
    now_us()
}

/// Real-time loop: calls `f` every period (drift-scaled), sleeping until
/// absolute deadlines so there is no cumulative error.
fn rt_loop(
    stop: Arc<AtomicBool>,
    gap_ns: Arc<AtomicU64>,
    period: Duration,
    mut f: impl FnMut(Instant) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut n: u32 = 0;
        let mut last: Option<Instant> = None;
        while !stop.load(Ordering::SeqCst) {
            let due = t0 + period * n;
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            let now = Instant::now();
            if let Some(l) = last {
                let g = (now - l).as_nanos() as u64;
                gap_ns.fetch_max(g, Ordering::SeqCst);
            }
            last = Some(now);
            f(now);
            n += 1;
        }
    })
}

pub fn open_producer(
    sim: Arc<AudioSim>,
    kind: Kind,
    _spec: AudioSpec,
    mut sink: PcmSink,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    register(&sim, kind, ev);
    let stop = Arc::new(AtomicBool::new(false));
    let gap_ns = Arc::new(AtomicU64::new(0));
    let ppm = sim.drift_ppm[kind.idx()].load(Ordering::SeqCst);
    let period = Duration::from_secs_f64(PERIOD as f64 / RATE / (1.0 + ppm as f64 / 1e6));
    let s = sim.clone();
    let mut phase = 0.0f64;
    let mut pos: u64 = 0;
    let mut buf = vec![0i16; PERIOD * 2];
    let join = rt_loop(stop.clone(), gap_ns.clone(), period, move |_now| {
        if kind == Kind::VirtualSpeaker && !s.app_playing.load(Ordering::SeqCst) {
            return; // PipeWire delivers nothing while no app plays to the sink
        }
        let wall = now_wall_us();
        for i in 0..PERIOD {
            let p = (pos + i as u64) % MARKER_EVERY;
            let amp = if (p as usize) < MARKER_LEN {
                let p = p as usize;
                let ramp = if p < MARKER_RAMP {
                    p as f64 / MARKER_RAMP as f64
                } else if p >= MARKER_LEN - MARKER_RAMP {
                    (MARKER_LEN - p) as f64 / MARKER_RAMP as f64
                } else {
                    1.0
                };
                BASE_AMP + (MARKER_AMP - BASE_AMP) * ramp
            } else {
                BASE_AMP
            };
            if p == 0 {
                // wall clock of this sample: the callback delivers the
                // period's last sample "now".
                let t = wall.saturating_sub(((PERIOD - i) as f64 / RATE * 1e6) as u64);
                let mut m = s.markers[kind.idx()].lock().unwrap();
                m.push_back(t);
                while m.len() > 64 {
                    m.pop_front();
                }
            }
            let v = (amp * phase.sin()) as i16;
            phase += 2.0 * std::f64::consts::PI * 440.0 / RATE;
            if phase > 2.0 * std::f64::consts::PI {
                phase -= 2.0 * std::f64::consts::PI;
            }
            buf[2 * i] = v;
            buf[2 * i + 1] = v;
        }
        pos += PERIOD as u64;
        s.produced[kind.idx()].fetch_add(PERIOD as u64, Ordering::SeqCst);
        sink(&buf);
    });
    Ok(Box::new(Dev {
        stop,
        join: Some(join),
        kind,
        sim,
        gap_ns,
    }))
}

pub fn open_consumer(
    sim: Arc<AudioSim>,
    kind: Kind,
    _spec: AudioSpec,
    mut src: PcmSource,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    register(&sim, kind, ev);
    let stop = Arc::new(AtomicBool::new(false));
    let gap_ns = Arc::new(AtomicU64::new(0));
    let period = Duration::from_secs_f64(PERIOD as f64 / RATE);
    let s = sim.clone();
    let mut buf = vec![0i16; PERIOD * 2];
    let join = rt_loop(stop.clone(), gap_ns.clone(), period, move |now| {
        if kind == Kind::VirtualMic && !s.mic_consumer.load(Ordering::SeqCst) {
            return; // nobody records: PipeWire doesn't schedule the node
        }
        src(&mut buf);
        let wall = now_wall_us();
        let mut m = s.meters[kind.idx()].lock().unwrap();
        for i in 0..PERIOD {
            let v = buf[2 * i];
            m.samples += 1;
            if v != 0 {
                m.nonzero_samples += 1;
                if !m.started {
                    m.started = true;
                    m.prev = v;
                }
                if m.zero_run >= DROPOUT_SAMPLES {
                    m.dropouts += 1;
                    let ms = m.zero_run as f64 / 48.0;
                    m.longest_dropout_ms = m.longest_dropout_ms.max(ms);
                }
                m.zero_run = 0;
            } else if m.started {
                m.zero_run += 1;
            }
            if v.unsigned_abs() > MARKER_THRESHOLD as u16 {
                m.in_marker = MARKER_LEN as u32;
                let fresh = m
                    .last_detect
                    .is_none_or(|t| now.duration_since(t) > Duration::from_millis(500));
                if fresh {
                    m.last_detect = Some(now);
                    // This sample leaves the device at about `wall` (callback
                    // time) + its offset in the buffer.
                    let heard = wall + (i as f64 / RATE * 1e6) as u64;
                    let mut mk = s.markers[kind.producer().idx()].lock().unwrap();
                    // The marker that was produced most recently before `heard`.
                    while mk.len() > 1 && mk[1] <= heard {
                        mk.pop_front();
                    }
                    if let Some(&t) = mk.front() {
                        if t <= heard && heard - t < 1_000_000 {
                            m.latencies_ms.push((heard - t) as f64 / 1000.0);
                        }
                        mk.pop_front();
                    }
                }
            }
            if m.in_marker > 0 {
                m.in_marker -= 1;
            } else if m.started && v != 0 && m.prev != 0 {
                let j = (v as i32 - m.prev as i32).abs();
                m.max_jump = m.max_jump.max(j);
            }
            m.prev = v;
        }
    });
    Ok(Box::new(Dev {
        stop,
        join: Some(join),
        kind,
        sim,
        gap_ns,
    }))
}
