//! "Zoom" for a real end-to-end run against two running `linkerd`s
//! (see `runbook/audio-e2e.sh`): plays a tone with latency markers into
//! "Linker Speaker" and records "Linker Microphone", for `LINKER_E2E_SECS`
//! seconds, then prints round-trip latency and a timeline of every silent
//! interval (so link drops / sleep-wake recovery can be measured).
//!
//! Run: `LINKER_E2E=1 cargo test -p linker-linux --test e2e_daemons -- --ignored --nocapture`
#![cfg(all(target_os = "linux", feature = "audio"))]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linker_linux::LinuxBackend;
use linker_platform::{AudioSpec, PlatformBackend};

const MARKER_EVERY: u64 = 48_000;
const MARKER_LEN: u64 = 1_440;

#[derive(Default)]
struct Rec {
    samples: u64,
    started: bool,
    silent_from: Option<f64>,
    zero_run: u64,
    gaps: Vec<(f64, f64)>,
    lat_ms: Vec<(f64, f64)>,
    last_detect: Option<Instant>,
    last_cb: Option<f64>,
    /// Recorder callbacks that came > 40 ms apart: (t, gap ms).
    cb_gaps: Vec<(f64, f64)>,
}

#[test]
#[ignore = "needs two running linkerd daemons (runbook/audio-e2e.sh)"]
fn zoom_like_app() {
    if std::env::var("LINKER_E2E").is_err() {
        eprintln!("set LINKER_E2E=1");
        return;
    }
    let secs: u64 = std::env::var("LINKER_E2E_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let speaker = std::env::var("LINKER_E2E_SPEAKER").unwrap_or_else(|_| "linker_speaker".into());
    let mic = std::env::var("LINKER_E2E_MIC").unwrap_or_else(|_| "linker_microphone".into());
    let b = LinuxBackend::new();
    let (ev, _ev_rx) = tokio::sync::mpsc::channel(16);
    let t0 = Instant::now();
    let markers: Arc<Mutex<VecDeque<u64>>> = Arc::default();
    let rec = Arc::new(Mutex::new(Rec::default()));

    let _capture = {
        let (rec, markers) = (rec.clone(), markers.clone());
        b.audio_capture(
            AudioSpec {
                device: Some(mic.clone()),
                description: String::new(),
                period_ms: 2.67,
            },
            Box::new(move |pcm: &[i16]| {
                let now = Instant::now();
                let t_end = (now - t0).as_secs_f64();
                let n = pcm.len() / 2;
                let mut r = rec.lock().unwrap();
                if let Some(l) = r.last_cb {
                    if t_end - l > 0.040 {
                        r.cb_gaps.push((t_end, (t_end - l) * 1000.0));
                    }
                }
                r.last_cb = Some(t_end);
                for i in 0..n {
                    let v = pcm[2 * i];
                    let t = t_end - (n - i) as f64 / 48_000.0;
                    r.samples += 1;
                    if v == 0 {
                        if r.started {
                            r.zero_run += 1;
                            if r.zero_run == 96 {
                                r.silent_from = Some(t - 0.002);
                            }
                        }
                    } else {
                        r.started = true;
                        if let Some(from) = r.silent_from.take() {
                            r.gaps.push((from, t));
                        }
                        r.zero_run = 0;
                    }
                    if v.unsigned_abs() > 15_000
                        && r.last_detect
                            .is_none_or(|d| now - d > Duration::from_millis(500))
                    {
                        r.last_detect = Some(now);
                        let heard = (t * 1e9) as u64;
                        let mut mk = markers.lock().unwrap();
                        while mk.len() > 1 && mk[1] <= heard {
                            mk.pop_front();
                        }
                        if let Some(&m) = mk.front() {
                            if m <= heard && heard - m < 2_000_000_000 {
                                r.lat_ms.push((t, (heard - m) as f64 / 1e6));
                            }
                            mk.pop_front();
                        }
                    }
                }
            }),
            ev.clone(),
        )
        .expect("record from Linker Microphone")
    };
    let play_gaps: Arc<Mutex<Vec<(f64, f64)>>> = Arc::default();
    let _play = {
        let markers = markers.clone();
        let play_gaps = play_gaps.clone();
        let mut last_cb: Option<f64> = None;
        let (mut pos, mut phase) = (0u64, 0.0f64);
        b.audio_playback(
            AudioSpec {
                device: Some(speaker.clone()),
                description: String::new(),
                period_ms: 2.67,
            },
            Box::new(move |out: &mut [i16]| {
                let now = (Instant::now() - t0).as_nanos() as u64;
                let tn = now as f64 / 1e9;
                if let Some(l) = last_cb {
                    if tn - l > 0.040 {
                        play_gaps.lock().unwrap().push((tn, (tn - l) * 1000.0));
                    }
                }
                last_cb = Some(tn);
                let n = out.len() / 2;
                for i in 0..n {
                    let p = (pos + i as u64) % MARKER_EVERY;
                    if p == 0 {
                        markers
                            .lock()
                            .unwrap()
                            .push_back(now + i as u64 * 1_000_000_000 / 48_000);
                    }
                    let amp = if p < MARKER_LEN { 22_000.0 } else { 6_000.0 };
                    let v = (amp * phase.sin()) as i16;
                    phase += 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
                    if phase > 2.0 * std::f64::consts::PI {
                        phase -= 2.0 * std::f64::consts::PI;
                    }
                    out[2 * i] = v;
                    out[2 * i + 1] = v;
                }
                pos += n as u64;
            }),
            ev.clone(),
        )
        .expect("play into Linker Speaker")
    };

    for s in 0..secs {
        std::thread::sleep(Duration::from_secs(1));
        if s % 10 == 9 {
            let r = rec.lock().unwrap();
            eprintln!(
                "t={:>4}s recorded {:.1}s, gaps so far {}",
                s + 1,
                r.samples as f64 / 48_000.0,
                r.gaps.len()
            );
        }
    }
    let mut r = std::mem::take(&mut *rec.lock().unwrap());
    if let Some(from) = r.silent_from.take() {
        r.gaps.push((from, (Instant::now() - t0).as_secs_f64()));
    }
    let mut lat: Vec<f64> = r.lat_ms.iter().map(|x| x.1).collect();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| {
        lat.get(((lat.len() as f64 - 1.0) * p).round() as usize)
            .copied()
            .unwrap_or(f64::NAN)
    };
    eprintln!("=== E2E: {speaker} → (flow B) → peer → (flow A) → {mic}");
    eprintln!("recorded {:.1} s", r.samples as f64 / 48_000.0);
    eprintln!(
        "round-trip latency over {} markers: min {:.1} / median {:.1} / p95 {:.1} / max {:.1} ms",
        lat.len(),
        pct(0.0),
        pct(0.5),
        pct(0.95),
        pct(1.0)
    );
    eprintln!("silent intervals (≥ 2 ms): {}", r.gaps.len());
    for (a, b) in &r.gaps {
        eprintln!("  {a:8.3}s – {b:8.3}s  ({:.0} ms)", (b - a) * 1000.0);
    }
    eprintln!(
        "recorder callback gaps > 40 ms (t, ms): {:?}",
        r.cb_gaps
            .iter()
            .map(|(t, g)| (format!("{t:.2}"), g.round() as u64))
            .collect::<Vec<_>>()
    );
    eprintln!(
        "player callback gaps > 40 ms (t, ms): {:?}",
        play_gaps
            .lock()
            .unwrap()
            .iter()
            .map(|(t, g)| (format!("{t:.2}"), g.round() as u64))
            .collect::<Vec<_>>()
    );
    eprintln!(
        "latency timeline (t, ms): {:?}",
        r.lat_ms
            .iter()
            .map(|(t, l)| (t.round() as u64, l.round() as u64))
            .collect::<Vec<_>>()
    );
}
