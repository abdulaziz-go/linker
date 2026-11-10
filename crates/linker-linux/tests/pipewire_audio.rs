//! Real PipeWire end-to-end test (needs the user's PipeWire session):
//!
//! ```text
//! tone ─► playback stream (target linker_speaker, like Zoom's output)
//!          ─► "Linker Speaker" virtual sink ─► Sender (Opus) ─► NetSim ─► Receiver ─► JitterBuffer
//!          ─► "Linker Microphone" virtual source ─► capture stream (target linker_microphone, like Zoom's mic)
//! ```
//!
//! Run: `cargo test -p linker-linux --test pipewire_audio -- --ignored --nocapture --test-threads=1`
//! (`LINKER_PW_SECS` = seconds of steady-state measurement, default 15;
//! `LINKER_PW_IMPAIR` = NetSim spec, default a lossy Wi-Fi-like link).
#![cfg(all(target_os = "linux", feature = "audio"))]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linker_audio::jitter::JitterBuffer;
use linker_audio::proto;
use linker_audio::receiver::Receiver;
use linker_audio::sender::{Sender, SenderParams};
use linker_audio::sim::{Impair, NetSim};
use linker_linux::LinuxBackend;
use linker_platform::{AudioSpec, PlatformBackend};
use tokio::sync::mpsc;

const MARKER_EVERY: u64 = 48_000;
const MARKER_LEN: u64 = 1_440;

fn pw_dump_has(node_name: &str, class: &str) -> bool {
    let out = std::process::Command::new("pw-dump").output();
    let Ok(out) = out else { return false };
    let s = String::from_utf8_lossy(&out.stdout);
    s.contains(&format!("\"node.name\": \"{node_name}\""))
        && s.contains(&format!("\"media.class\": \"{class}\""))
}

#[derive(Default)]
struct Meter {
    started: bool,
    samples: u64,
    nonzero: u64,
    zero_run: u64,
    dropouts: u32,
    lat_ms: Vec<f64>,
    last_detect: Option<Instant>,
}

#[test]
#[ignore = "needs a running PipeWire session"]
fn speaker_to_microphone_through_the_engine() {
    let secs: u64 = std::env::var("LINKER_PW_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let impair = std::env::var("LINKER_PW_IMPAIR")
        .unwrap_or_else(|_| "loss=3,reorder=5,jitter=3,seed=1".into());
    let b = LinuxBackend::new();
    let (ev_tx, mut ev_rx) = mpsc::channel(16);

    // --- network + engine between the two virtual devices ---
    let jb = Arc::new(Mutex::new(JitterBuffer::new(48_000, 2, 6, 200)));
    let rx = Arc::new(Mutex::new(Receiver::new(jb.clone()).unwrap()));
    let net = Arc::new(Mutex::new(NetSim::new(Impair::parse(&impair).unwrap())));
    let tx = Arc::new(Mutex::new(
        Sender::new(proto::TAG_SPEAKER, SenderParams::default()).unwrap(),
    ));

    // Marker bookkeeping (wall-clock ns since t0).
    let t0 = Instant::now();
    let markers = Arc::new(Mutex::new(std::collections::VecDeque::<u64>::new()));
    let meter = Arc::new(Mutex::new(Meter::default()));

    // 1. Linker Speaker: what apps play is encoded and sent into the NetSim.
    let speaker = {
        let (tx, net) = (tx.clone(), net.clone());
        b.virtual_speaker(
            AudioSpec {
                device: Some("linker_speaker".into()),
                description: "Linker Speaker".into(),
                period_ms: 2.67,
            },
            Box::new(move |pcm: &[i16]| {
                let now = Instant::now();
                let mut net = net.lock().unwrap();
                tx.lock().unwrap().push(pcm, 0, |d| {
                    let (_, _, p) = proto::unwrap(d).unwrap();
                    net.send(now, p);
                });
            }),
            ev_tx.clone(),
        )
        .expect("virtual speaker")
    };
    // 2. Linker Microphone: apps record what the jitter buffer plays.
    let mic = {
        let jb = jb.clone();
        b.virtual_mic(
            AudioSpec {
                device: Some("linker_microphone".into()),
                description: "Linker Microphone".into(),
                period_ms: 2.67,
            },
            Box::new(move |out: &mut [i16]| jb.lock().unwrap().pull(out, Instant::now())),
            ev_tx.clone(),
        )
        .expect("virtual mic")
    };
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        pw_dump_has("linker_speaker", "Audio/Sink"),
        "Linker Speaker not visible in pw-dump"
    );
    assert!(
        pw_dump_has("linker_microphone", "Audio/Source"),
        "Linker Microphone not visible in pw-dump"
    );
    assert!(
        !mic.active(),
        "no app records yet → virtual mic must be idle"
    );

    // 3. Network delivery thread (5 ms deadline loop like the receiver).
    let stop = Arc::new(AtomicBool::new(false));
    let net_thread = {
        let (net, rx, stop) = (net.clone(), rx.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let now = Instant::now();
                let mut pkts = Vec::new();
                net.lock().unwrap().poll(now, |p| pkts.push(p.to_vec()));
                let mut r = rx.lock().unwrap();
                for p in pkts {
                    let _ = r.on_packet(&p, now);
                }
                r.on_tick(now);
                drop(r);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };

    // 4. "Zoom" records from Linker Microphone …
    let rec = {
        let (meter, markers) = (meter.clone(), markers.clone());
        b.audio_capture(
            AudioSpec {
                device: Some("linker_microphone".into()),
                description: String::new(),
                period_ms: 2.67,
            },
            Box::new(move |pcm: &[i16]| {
                let now = Instant::now();
                let mut m = meter.lock().unwrap();
                for (i, f) in pcm.as_chunks::<2>().0.iter().enumerate() {
                    let v = f[0];
                    m.samples += 1;
                    if v != 0 {
                        m.nonzero += 1;
                        m.started = true;
                        if m.zero_run >= 96 {
                            m.dropouts += 1;
                        }
                        m.zero_run = 0;
                    } else if m.started {
                        m.zero_run += 1;
                    }
                    if v.unsigned_abs() > 15_000
                        && m.last_detect
                            .is_none_or(|t| now - t > Duration::from_millis(500))
                    {
                        m.last_detect = Some(now);
                        // sample i of this buffer was captured ~ (n - i)/48k before now
                        let heard = (now - t0).as_nanos() as u64
                            - ((pcm.len() / 2 - i) as u64 * 1_000_000_000 / 48_000);
                        let mut mk = markers.lock().unwrap();
                        while mk.len() > 1 && mk[1] <= heard {
                            mk.pop_front();
                        }
                        if let Some(&t) = mk.front() {
                            if t <= heard {
                                m.lat_ms.push((heard - t) as f64 / 1e6);
                            }
                            mk.pop_front();
                        }
                    }
                }
            }),
            ev_tx.clone(),
        )
        .expect("capture from Linker Microphone")
    };
    // … and plays a tone into Linker Speaker.
    let pos = Arc::new(AtomicU64::new(0));
    let play = {
        let (pos, markers) = (pos.clone(), markers.clone());
        let mut phase = 0.0f64;
        b.audio_playback(
            AudioSpec {
                device: Some("linker_speaker".into()),
                description: String::new(),
                period_ms: 2.67,
            },
            Box::new(move |out: &mut [i16]| {
                let now_ns = (Instant::now() - t0).as_nanos() as u64;
                let n = out.len() / 2;
                let p0 = pos.fetch_add(n as u64, Ordering::SeqCst);
                for i in 0..n {
                    let p = (p0 + i as u64) % MARKER_EVERY;
                    if p == 0 {
                        // this sample leaves our process ~now (+ i samples)
                        markers
                            .lock()
                            .unwrap()
                            .push_back(now_ns + i as u64 * 1_000_000_000 / 48_000);
                    }
                    let amp = if p < MARKER_LEN { 22_000.0 } else { 6_000.0 };
                    let v = (amp * phase.sin()) as i16;
                    phase += 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
                    out[2 * i] = v;
                    out[2 * i + 1] = v;
                }
            }),
            ev_tx.clone(),
        )
        .expect("play into Linker Speaker")
    };

    // Demand detection: the virtual mic is now scheduled.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !mic.active() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        mic.active(),
        "recording app present → virtual mic must report active"
    );
    assert!(
        speaker.active(),
        "playing app present → virtual speaker must report active"
    );

    // Warm-up, then measure.
    std::thread::sleep(Duration::from_secs(4));
    let under0 = jb.lock().unwrap().stats().underruns;
    *meter.lock().unwrap() = Meter {
        started: true,
        ..Default::default()
    };
    std::thread::sleep(Duration::from_secs(secs));

    let m = std::mem::take(&mut *meter.lock().unwrap());
    let js = jb.lock().unwrap().stats();
    let rs = rx.lock().unwrap().stats();
    let mut lat = m.lat_ms.clone();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = lat.get(lat.len() / 2).copied();
    eprintln!("impairment      : {impair}");
    eprintln!(
        "recorded        : {:.1} s, non-zero {:.2} %, dropouts {}",
        m.samples as f64 / 48_000.0,
        100.0 * m.nonzero as f64 / m.samples.max(1) as f64,
        m.dropouts
    );
    eprintln!(
        "latency (marker): median {med:?} ms over {} markers, all {:?}",
        lat.len(),
        lat
    );
    eprintln!("receiver        : {rs:?}");
    eprintln!("jitter buffer   : {js:?}");
    eprintln!("pipewire        : speaker period {} frames, mic period {} frames, max cb gap speaker {:.1} ms mic {:.1} ms",
        speaker.info().period_frames, mic.info().period_frames, speaker.take_max_callback_gap_ms(), mic.take_max_callback_gap_ms());
    if let Ok(e) = ev_rx.try_recv() {
        panic!("unexpected device event: {e:?}");
    }

    stop.store(true, Ordering::SeqCst);
    drop(play);
    drop(rec);
    net_thread.join().unwrap();

    assert!(
        m.samples as f64 >= secs as f64 * 48_000.0 * 0.95,
        "recording starved"
    );
    assert_eq!(m.dropouts, 0, "audible gaps in Linker Microphone");
    assert_eq!(
        js.underruns - under0,
        0,
        "jitter-buffer underruns after warm-up"
    );
    assert!(m.nonzero as f64 >= 0.99 * m.samples as f64);
    let med = med.expect("markers detected");
    assert!(med < 120.0, "speaker→mic latency {med:.1} ms");

    // Devices vanish with their streams.
    drop(mic);
    drop(speaker);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !pw_dump_has("linker_speaker", "Audio/Sink"),
        "Linker Speaker left behind"
    );
}
