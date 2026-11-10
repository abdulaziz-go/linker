//! Full-chain simulation in virtual time: capture (440 Hz sine) → Sender
//! (Opus LOWDELAY, 5 ms, redundancy 2) → NetSim (impairment) → Receiver
//! (reorder, decode/PLC) → JitterBuffer → 128-frame playback callback.
//!
//! These are the Rust equivalents of running audiobridge's `testtone` with
//! `-jitter / -loss / -reorder / -stall` against the receiver, but
//! deterministic and fast. "No audible gaps" is checked three ways on the
//! played signal after warm-up: no run of ≥ 2 ms of digital silence, no
//! 10 ms window whose RMS falls below 30 % of the sine's RMS, and the
//! jitter buffer reports no underruns.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linker_audio::jitter::JitterBuffer;
use linker_audio::proto;
use linker_audio::receiver::Receiver;
use linker_audio::sender::{Sender, SenderParams};
use linker_audio::sim::{Impair, NetSim};

const AMP: f64 = 8000.0;

#[derive(Debug, Default)]
struct Outcome {
    played_s: f64,
    dropouts: u32,
    weak_windows: u32,
    underruns_after_warmup: u64,
    trims_after_warmup: u64,
    max_jump: i32,
    zero_crossings_per_s: f64,
    recovered: u64,
    plc: u64,
    late: u64,
    target_ms: u32,
    avg_ms: u32,
    speed_pct: f64,
    sent: u64,
    dropped: u64,
}

/// Run `secs` of virtual time; measurements start after `warmup` seconds.
/// `drift` scales the producer clock (1.002 = 0.2 % fast).
fn run(imp: &str, secs: f64, warmup: f64, drift: f64, min_ms: u32) -> Outcome {
    let imp = Impair::parse(imp).unwrap();
    let mut net = NetSim::new(imp);
    let jb = Arc::new(Mutex::new(JitterBuffer::new(48_000, 2, min_ms, 200)));
    let mut rx = Receiver::new(jb.clone()).unwrap();
    let mut tx = Sender::new(proto::TAG_MIC, SenderParams::default()).unwrap();

    let t0 = Instant::now();
    let step = Duration::from_micros(250);
    let steps = (secs / 0.000_25) as u64;
    let prod_period = 0.005 / drift; // 240 frames
    let cons_period = 128.0 / 48_000.0;
    let (mut next_prod, mut next_cons) = (0.0f64, 0.0f64);
    let mut phase = 0.0f64;
    let mut pcm = vec![0i16; 240 * 2];
    let mut out = vec![0i16; 128 * 2];
    let mut o = Outcome::default();
    let mut started = false;
    let mut zero_run = 0u32;
    let mut prev = 0i16;
    let (mut win_e, mut win_n) = (0.0f64, 0u32);
    let mut zc = 0u64;
    let mut measured = 0u64;
    let (mut under0, mut trim0) = (0, 0);
    let mut warm = false;

    for s in 0..steps {
        let t = s as f64 * 0.000_25;
        let now = t0 + step * s as u32;
        while next_prod <= t {
            for f in 0..240 {
                let v = (AMP * phase.sin()) as i16;
                phase += 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
                pcm[2 * f] = v;
                pcm[2 * f + 1] = v;
            }
            tx.push(&pcm, 0, |d| {
                let (_, _, p) = proto::unwrap(d).unwrap();
                net.send(now, p);
            });
            next_prod += prod_period;
        }
        net.poll(now, |p| {
            let _ = rx.on_packet(p, now);
        });
        rx.on_tick(now);
        while next_cons <= t {
            jb.lock().unwrap().pull(&mut out, now);
            next_cons += cons_period;
            if !warm && t >= warmup {
                warm = true;
                let st = jb.lock().unwrap().stats();
                under0 = st.underruns;
                trim0 = st.trimmed;
            }
            for f in 0..128 {
                let v = out[2 * f];
                if v != 0 {
                    started = true;
                }
                if !warm {
                    prev = v;
                    continue;
                }
                measured += 1;
                if v == 0 && started {
                    zero_run += 1;
                    if zero_run == 96 {
                        o.dropouts += 1;
                    }
                } else {
                    zero_run = 0;
                }
                o.max_jump = o.max_jump.max((v as i32 - prev as i32).abs());
                if (v < 0) != (prev < 0) {
                    zc += 1;
                }
                prev = v;
                win_e += v as f64 * v as f64;
                win_n += 1;
                if win_n == 480 {
                    let rms = (win_e / 480.0).sqrt();
                    if rms < 0.3 * AMP / std::f64::consts::SQRT_2 {
                        o.weak_windows += 1;
                    }
                    win_e = 0.0;
                    win_n = 0;
                }
            }
        }
    }
    let js = jb.lock().unwrap().stats();
    let rs = rx.stats();
    let ns = net.stats();
    o.played_s = measured as f64 / 48_000.0;
    o.zero_crossings_per_s = zc as f64 / o.played_s.max(1e-9);
    o.underruns_after_warmup = js.underruns - under0;
    o.trims_after_warmup = js.trimmed - trim0;
    o.recovered = rs.recovered;
    o.plc = rs.plc;
    o.late = rs.late;
    o.target_ms = js.target_ms;
    o.avg_ms = js.avg_ms;
    o.speed_pct = js.speed_pct;
    o.sent = ns.sent;
    o.dropped = ns.dropped;
    eprintln!("{o:?}");
    o
}

fn assert_no_gaps(o: &Outcome) {
    assert_eq!(o.dropouts, 0, "silence gaps: {o:?}");
    assert_eq!(o.weak_windows, 0, "faded/empty 10 ms windows: {o:?}");
    assert_eq!(o.underruns_after_warmup, 0, "underruns: {o:?}");
    // 440 Hz → 880 zero crossings per second; a gap or a skip shifts this.
    assert!(
        (o.zero_crossings_per_s - 880.0).abs() < 880.0 * 0.01,
        "pitch/continuity off: {o:?}"
    );
}

#[test]
fn clean_network() {
    let o = run("", 20.0, 1.0, 1.0, 6);
    assert_no_gaps(&o);
    assert_eq!((o.plc, o.recovered, o.late), (0, 0, 0));
    assert!(o.max_jump < 1500, "click: {o:?}");
    // Minimal latency: buffer stays near the 6 ms minimum + one 5 ms frame.
    assert!(o.avg_ms <= 15, "{o:?}");
}

#[test]
fn jitter_mode() {
    let o = run("jitter=8,seed=1", 30.0, 3.0, 1.0, 6);
    assert_no_gaps(&o);
    assert!(o.target_ms >= 8 && o.target_ms <= 30, "{o:?}");
}

#[test]
fn loss_mode_recovered_by_redundancy() {
    let o = run("loss=5,seed=2", 30.0, 1.0, 1.0, 6);
    assert_no_gaps(&o);
    assert!(
        o.recovered > 100,
        "redundancy must recover lost frames: {o:?}"
    );
    // 3 consecutive losses needed for PLC: ~0.0125 % of 6000 frames.
    assert!(o.plc <= 5, "{o:?}");
}

#[test]
fn heavy_loss_is_concealed_not_silent() {
    // 20 % random loss: runs of ≥ 3 lost packets happen, PLC conceals them.
    // (At 25 % a new longest gap still appears now and then: measured 2
    // underruns in 250 s — see `loss_sweep` and runbook/measurements.md.)
    let o = run("loss=20,seed=3", 40.0, 10.0, 1.0, 6);
    assert!(o.plc > 0, "{o:?}");
    // PLC fills every hole with audio — still no silence gaps.
    assert_eq!(o.dropouts, 0, "{o:?}");
    assert_eq!(o.underruns_after_warmup, 0, "{o:?}");
}

#[test]
fn reorder_mode_nothing_lost() {
    let o = run("reorder=20,seed=4", 30.0, 1.0, 1.0, 6);
    assert_no_gaps(&o);
    assert_eq!(o.plc, 0, "reordered packets must not become PLC: {o:?}");
}

#[test]
fn stall_mode_awdl_adapts() {
    // Wi-Fi stops for 100 ms every second, then delivers everything at once.
    let o = run("stall_every=1000,stall_ms=100,seed=5", 30.0, 4.0, 1.0, 6);
    assert_no_gaps(&o);
    assert!(o.target_ms >= 100 && o.target_ms <= 140, "{o:?}");
    assert_eq!(o.plc, 0, "{o:?}");
}

#[test]
fn everything_at_once() {
    let o = run(
        "loss=5,reorder=10,jitter=5,stall_every=1500,stall_ms=100,seed=6",
        40.0,
        5.0,
        1.0,
        6,
    );
    assert_no_gaps(&o);
    assert!(o.plc <= 10, "{o:?}");
}

#[test]
fn clock_drift_both_ways_is_absorbed() {
    // Sender's audio clock 0.2 % fast: buffer must not grow without bound.
    let fast = run("jitter=3,seed=7", 60.0, 3.0, 1.002, 6);
    assert_no_gaps(&fast);
    // Absorbed by the ≤ +0.5 % speed-up (it returns to 1.0× inside the
    // deadband, so the instantaneous speed may read 0): the level stays
    // bounded and no crossfade trim was needed.
    assert!(fast.avg_ms <= 40, "buffer grew: {fast:?}");
    assert_eq!(fast.trims_after_warmup, 0, "{fast:?}");
    // Slow sender (−500 ppm; real Mac/PC audio clocks differ by ~10–100):
    // must not underrun. (audiobridge's original slow-down threshold
    // underran here — see the DEVIATION note in jitter.rs.)
    let slow = run("jitter=3,seed=8", 60.0, 3.0, 0.9995, 6);
    assert_eq!(slow.underruns_after_warmup, 0, "{slow:?}");
    assert_eq!(slow.dropouts, 0, "{slow:?}");
}

#[test]
#[ignore = "measurement: drift sweep (cargo test --test sim drift_sweep -- --ignored --nocapture)"]
fn drift_sweep() {
    for ppm in [-2000i32, -1000, -500, -200, -100, 100, 200, 500, 1000, 2000] {
        let o = run("jitter=3,seed=9", 180.0, 3.0, 1.0 + ppm as f64 / 1e6, 6);
        eprintln!(
            "drift {ppm:+5} ppm: underruns {} dropouts {} trims {} avg {} ms target {} ms",
            o.underruns_after_warmup, o.dropouts, o.trims_after_warmup, o.avg_ms, o.target_ms
        );
    }
}

#[test]
#[ignore = "measurement: loss sweep"]
fn loss_sweep() {
    for loss in [5, 10, 15, 20, 25] {
        let mut tot = (0, 0, 0);
        for seed in 1..=5 {
            let o = run(&format!("loss={loss},seed={seed}"), 60.0, 10.0, 1.0, 6);
            tot.0 += o.underruns_after_warmup;
            tot.1 += o.plc;
            tot.2 += o.target_ms;
        }
        eprintln!(
            "loss {loss:2}%: underruns {} in 5x50 s, PLC frames {}, mean target {} ms",
            tot.0,
            tot.1,
            tot.2 / 5
        );
    }
}
