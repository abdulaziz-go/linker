//! Adaptive jitter buffer — port of audiobridge `cmd/receiver/jitter.go`.
//!
//! An adaptive ring buffer of interleaved `i16` that swallows Wi-Fi jitter:
//!
//! * The target level tunes itself: it measures the largest pause between
//!   packets over the last 30 s and sizes the buffer so it can ride out that
//!   pause (clamped to `min..max`). A real underrun raises the target further.
//! * Audio that piles up beyond the target is dropped mostly in QUIET places
//!   (inaudible). If the music is continuous, playback speed is raised very
//!   slowly (≤ 0.5 %).
//! * Underruns / holes get a fade-out / fade-in; a huge burst is cut with a
//!   crossfade.
//!
//! Unlike the Go type, [`JitterBuffer`] is not internally locked: the owner
//! wraps it in a `Mutex` (push happens on the network thread, pull in the
//! real-time audio callback).

use std::time::{Duration, Instant};

/// Length of every fade (fade-in, fade-out, crossfade), in milliseconds.
pub const FADE_MS: usize = 3;
/// A pause between pushes longer than this means the sender PC went silent,
/// not network jitter — it is not counted as a gap.
pub const IGNORE_GAP: Duration = Duration::from_millis(400);
/// An underrun counts as real (and raises the target) only if a push happened
/// less than this long ago, i.e. the stream is still flowing.
pub const ACTIVE_GAP: Duration = Duration::from_millis(150);
/// Maximum playback speed-up (+0.5 %).
pub const MAX_SPEED_UP: f64 = 0.005;
/// Maximum playback slow-down (−0.3 %).
pub const MAX_SLOW_DOWN: f64 = 0.003;
/// ~ −47 dBFS: frames whose every channel is within ±this are "quiet".
pub const QUIET_LEVEL: i16 = 150;

/// Number of one-second gap buckets (the gap history window, in seconds).
const GAP_BUCKETS: usize = 30;

/// Snapshot of the buffer state for logging / UI (`jbStats` in Go, plus
/// `playing`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JbStats {
    /// Current level (ms).
    pub level_ms: u32,
    /// ~2 s smoothed level (ms).
    pub avg_ms: u32,
    /// Adaptive target level (ms).
    pub target_ms: u32,
    /// Largest inter-push gap over the last 30 s (ms).
    pub peak_gap_ms: u32,
    /// Playback speed deviation in percent (`+0.5` = 0.5 % faster).
    pub speed_pct: f64,
    pub underruns: u64,
    pub trimmed: u64,
    pub quiet_skips: u64,
    /// `false` while (re)filling up to the target, i.e. outputting silence.
    pub playing: bool,
}

/// Adaptive jitter buffer (see the module docs).
#[derive(Debug)]
pub struct JitterBuffer {
    buf: Vec<i16>,
    /// Read index (sample).
    r: usize,
    /// Samples in the buffer.
    n: usize,

    ch: usize,
    /// Frames per millisecond.
    fpm: usize,

    /// Frames.
    min_target: usize,
    /// Frames.
    max_target: usize,
    /// Frames, adaptive.
    target: usize,

    /// Origin of the one-second gap buckets (Go used `time.Unix()`).
    epoch: Instant,
    last_push: Option<Instant>,
    /// Largest pause between pushes (ms) per one-second bucket.
    gap_max: [i64; GAP_BUCKETS],
    /// Which second (since `epoch`) each bucket belongs to.
    gap_sec: [i64; GAP_BUCKETS],
    peak_gap: i64,

    playing: bool,
    /// Fractional position inside the current frame.
    pos: f64,
    /// Playback speed (1.0 = normal).
    ratio: f64,
    /// Smoothed buffer level (frames).
    avg: f64,
    fade_in: usize,
    /// Old audio for the crossfade after a trim.
    xold: Vec<f32>,
    xleft: usize,

    underruns: u64,
    trimmed: u64,
    quiet_skips: u64,
}

impl JitterBuffer {
    /// A 2-second ring for `rate` Hz × `ch` channels; the target adapts
    /// within `min_ms..=max_ms` and starts at `min_ms`.
    pub fn new(rate: u32, ch: usize, min_ms: u32, max_ms: u32) -> JitterBuffer {
        let fpm = rate as usize / 1000;
        JitterBuffer {
            buf: vec![0; 2 * rate as usize * ch], // 2 seconds
            r: 0,
            n: 0,
            ch,
            fpm,
            min_target: min_ms as usize * fpm,
            max_target: max_ms as usize * fpm,
            target: min_ms as usize * fpm,
            epoch: Instant::now(),
            last_push: None,
            gap_max: [0; GAP_BUCKETS],
            // Far enough in the past that every bucket starts out stale.
            gap_sec: [-(GAP_BUCKETS as i64); GAP_BUCKETS],
            peak_gap: 0,
            playing: false,
            pos: 0.0,
            ratio: 1.0,
            avg: 0.0,
            fade_in: 0,
            xold: vec![0.0; FADE_MS * fpm * ch],
            xleft: 0,
            underruns: 0,
            trimmed: 0,
            quiet_skips: 0,
        }
    }

    /// Whole seconds since `epoch` (0 if `now` is before it) — the Go
    /// `now.Unix()` bucket key.
    fn sec(&self, now: Instant) -> i64 {
        now.saturating_duration_since(self.epoch).as_secs() as i64
    }

    /// Appends a chunk. `disc = true`: audio was lost just before `s`, so the
    /// seam is smoothed (fade-out of the unplayed tail, fade-in of `s`).
    ///
    /// If the ring overflows, the oldest audio is dropped.
    pub fn push(&mut self, s: &[i16], now: Instant, disc: bool) {
        if let Some(lp) = self.last_push {
            // A negative Go gap (now < lastPush) can only ever max with 0, so
            // saturating to 0 is equivalent.
            let g = now.saturating_duration_since(lp);
            if g < IGNORE_GAP {
                self.note_gap(g.as_millis() as i64, self.sec(now));
            }
        }
        self.last_push = Some(now);

        let (ch, fade_len) = (self.ch, FADE_MS * self.fpm);
        let fade = disc && self.n / ch > fade_len + 2;
        if fade {
            // Fade out the last (not yet played) part of the buffer; the new
            // chunk is faded in below, after it is copied into the ring.
            let len = self.buf.len();
            for k in 0..fade_len {
                let g = (fade_len - k) as f32 / (fade_len + 1) as f32;
                for c in 0..ch {
                    let i = (self.r + self.n - (fade_len - k) * ch + c) % len;
                    self.buf[i] = (self.buf[i] as f32 * g) as i16;
                }
            }
        }

        let c = self.buf.len();
        // Go fades `s` before truncating it, so the fade gains depend on the
        // original length; `drop` samples are cut from the front.
        let orig_frames = s.len() / ch;
        let drop = s.len().saturating_sub(c);
        let s = &s[drop..];
        let over = (self.n + s.len()).saturating_sub(c);
        if over > 0 {
            self.r = (self.r + over) % c;
            self.n -= over;
        }
        let w = (self.r + self.n) % c;
        let k = s.len().min(c - w);
        self.buf[w..w + k].copy_from_slice(&s[..k]);
        self.buf[..s.len() - k].copy_from_slice(&s[k..]);
        self.n += s.len();

        if fade {
            // fadeInSlice, applied to the copy in the ring (Go fades the
            // caller's slice in place; the audible result is identical).
            let nf = orig_frames.min(fade_len);
            for k in 0..nf {
                let g = (k + 1) as f32 / (nf + 1) as f32;
                for cc in 0..ch {
                    let si = k * ch + cc;
                    if si < drop {
                        continue; // this sample was truncated away
                    }
                    let i = (w + si - drop) % c;
                    self.buf[i] = (self.buf[i] as f32 * g) as i16;
                }
            }
        }
    }

    /// Adapts the target to the largest pause over the last 30 s.
    ///
    /// Normally packets arrive every few ms; to survive a pause of G ms the
    /// buffer must hold ~G ms.
    fn note_gap(&mut self, ms: i64, sec: i64) {
        let nb = GAP_BUCKETS as i64;
        let i = sec.rem_euclid(nb) as usize;
        if self.gap_sec[i] != sec {
            self.gap_sec[i] = sec;
            self.gap_max[i] = 0;
        }
        self.gap_max[i] = self.gap_max[i].max(ms);

        let mut peak = 0;
        for (k, &v) in self.gap_max.iter().enumerate() {
            if sec - self.gap_sec[k] < nb {
                peak = peak.max(v);
            }
        }
        self.peak_gap = peak;
        let t = (peak * self.fpm as i64).max(self.min_target as i64);
        self.target = t.min(self.max_target as i64) as usize;
    }

    fn at(&self, frame_off: usize, c: usize) -> f32 {
        self.buf[(self.r + frame_off * self.ch + c) % self.buf.len()] as f32
    }

    fn quiet(&self, frame_off: usize) -> bool {
        (0..self.ch).all(|c| {
            let v = self.buf[(self.r + frame_off * self.ch + c) % self.buf.len()];
            (-QUIET_LEVEL..=QUIET_LEVEL).contains(&v)
        })
    }

    fn advance(&mut self, frames: usize) {
        let s = (frames * self.ch).min(self.n);
        self.r = (self.r + s) % self.buf.len();
        self.n -= s;
    }

    /// Fills `out` (interleaved) — called from the audio callback. Outputs
    /// silence while (re)filling to the target.
    pub fn pull(&mut self, out: &mut [i16], now: Instant) {
        let ch = self.ch;
        let fpm = self.fpm as i64;
        let frames = out.len() / ch;
        let mut level = (self.n / ch) as i64;
        let fade_len = FADE_MS * self.fpm;
        let target = self.target as i64;

        if !self.playing {
            if level < target + frames as i64 {
                out.fill(0);
                return;
            }
            self.playing = true;
            self.fade_in = fade_len;
            self.pos = 0.0;
            self.ratio = 1.0;
            self.avg = level as f64;
        }

        // Emergency: the buffer overfilled — cut it, with a crossfade.
        let extra = level - target;
        if extra > (250 * fpm).max(target) && level > fade_len as i64 + 2 {
            for f in 0..fade_len {
                for c in 0..ch {
                    self.xold[f * ch + c] = self.at(f, c);
                }
            }
            // extra > 250 ms, so this leaves exactly 20 ms above the target.
            self.advance((extra - 20 * fpm) as usize);
            self.xleft = fade_len;
            self.trimmed += 1;
            level = (self.n / ch) as i64;
        }

        // Drop the excess in quiet places (at most 1 frame per output frame).
        // Signed: negative when the level is below target + 3 ms.
        let mut excess = level - target - 3 * fpm;

        // Speed: 1.0 inside the deadband, changes slowly only on a clear
        // deviation.
        //
        // DEVIATION from audiobridge (which slows down only below target/2):
        // with 5 ms packets and a 6 ms target the level's sawtooth already
        // touches zero before the smoothed level reaches target/2, so a sender
        // clock just 100 ppm slower than ours underran every ~90 s
        // (`tests/sim.rs` `drift_sweep`). Slowing down as soon as the smoothed
        // level is below the target keeps it at the target instead; the
        // ≤ 0.3 % (~5 cent) pitch change is inaudible, as in the original.
        let mut want = 1.0;
        if self.avg > (target + 10 * fpm) as f64 {
            want = 1.0 + MAX_SPEED_UP;
        } else if self.avg < target as f64 {
            want = 1.0 - MAX_SLOW_DOWN;
        }
        self.ratio += (want - self.ratio).clamp(-0.0001, 0.0001);

        for f in 0..frames {
            if self.n / ch < 3 {
                // Underrun: fade out what was produced, the rest is silence.
                fade_out_tail(&mut out[..f * ch], ch, fade_len);
                out[f * ch..].fill(0);
                self.playing = false;
                let flowing = self
                    .last_push
                    .is_some_and(|lp| now.saturating_duration_since(lp) < ACTIVE_GAP);
                if flowing {
                    // The stream is still going, so this is a real underrun:
                    // raise the target for the next 30 s.
                    self.underruns += 1;
                    let ms = (self.target / self.fpm) as i64 + 20;
                    self.note_gap(ms, self.sec(now));
                }
                return;
            }
            if excess > 0 && self.xleft == 0 && self.quiet(0) && self.quiet(1) {
                self.advance(1);
                excess -= 1;
                self.quiet_skips += 1;
            }
            let pos = self.pos as f32;
            let g = 1.0 - pos;
            for c in 0..ch {
                let mut v = self.at(0, c) * g + self.at(1, c) * pos;
                if self.xleft > 0 {
                    let t = self.xleft as f32 / fade_len as f32;
                    v = v * (1.0 - t) + self.xold[(fade_len - self.xleft) * ch + c] * t;
                }
                if self.fade_in > 0 {
                    v *= 1.0 - self.fade_in as f32 / fade_len as f32;
                }
                out[f * ch + c] = v as i16;
            }
            if self.xleft > 0 {
                self.xleft -= 1;
            }
            if self.fade_in > 0 {
                self.fade_in -= 1;
            }
            self.pos += self.ratio;
            let adv = self.pos as usize;
            if adv > 0 {
                self.advance(adv);
                self.pos -= adv as f64;
            }
        }

        // Level smoothed with a ~2 s time constant.
        let alpha = frames as f64 / (2000 * self.fpm) as f64;
        self.avg += ((self.n / ch) as f64 - self.avg) * alpha;
    }

    pub fn stats(&self) -> JbStats {
        JbStats {
            level_ms: (self.n / self.ch / self.fpm) as u32,
            avg_ms: (self.avg as i64 / self.fpm as i64) as u32,
            target_ms: (self.target / self.fpm) as u32,
            peak_gap_ms: self.peak_gap as u32,
            speed_pct: (self.ratio - 1.0) * 100.0,
            underruns: self.underruns,
            trimmed: self.trimmed,
            quiet_skips: self.quiet_skips,
            playing: self.playing,
        }
    }

    /// Buffered frames.
    pub fn level_frames(&self) -> usize {
        self.n / self.ch
    }

    /// Time of the last [`push`](Self::push), if any.
    pub fn last_push(&self) -> Option<Instant> {
        self.last_push
    }
}

/// Fades out the last `fade_len` frames of `s` (fewer if `s` is shorter).
fn fade_out_tail(s: &mut [i16], ch: usize, fade_len: usize) {
    let frames = s.len() / ch;
    let n = frames.min(fade_len);
    for k in 0..n {
        let gain = (n - k) as f32 / (n + 1) as f32;
        for c in 0..ch {
            let i = (frames - n + k) * ch + c;
            s[i] = (s[i] as f32 * gain) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const RATE: f64 = 48000.0;
    const AMP: f64 = 10000.0;

    /// Normal difference between consecutive samples of the test sine:
    /// 2π·440/48000·10000 ≈ 576.
    const SMOOTH_JUMP: i64 = 700;

    /// Result of [`sim`].
    struct SimResult {
        /// Largest jump between consecutive output samples after 2 s (a click).
        max_jump: i64,
        /// Underruns after the first 2 s.
        steady_underruns: u64,
    }

    /// Port of the Go `sim` helper: 10 ms chunks arrive, 5 ms callbacks read.
    /// With `stall_every`/`stall_ms`, Wi-Fi stops for `stall_ms` every
    /// `stall_every` ms and then delivers everything at once (like AWDL).
    fn sim(jb: &mut JitterBuffer, secs: u64, stall_every: u64, stall_ms: u64) -> SimResult {
        let t0 = Instant::now();
        let mut phase = 0.0f64;
        let mut pending: Vec<Vec<i16>> = Vec::new();
        let mut out = vec![0i16; 240 * 2];
        let mut prev = 0i16;
        let mut max_jump = 0i64;
        let mut underruns_at_2s = 0;
        for ms in (0..secs * 1000).step_by(5) {
            let now = t0 + Duration::from_millis(ms);
            if ms % 10 == 0 {
                let mut chunk = vec![0i16; 480 * 2];
                for f in 0..480 {
                    let v = (AMP * phase.sin()) as i16;
                    phase += 2.0 * PI * 440.0 / RATE;
                    chunk[2 * f] = v;
                    chunk[2 * f + 1] = v;
                }
                pending.push(chunk);
            }
            let stalled = stall_every > 0 && ms % stall_every < stall_ms;
            if !stalled {
                for c in pending.drain(..) {
                    jb.push(&c, now, false);
                }
            }
            jb.pull(&mut out, now);
            if ms == 2000 {
                underruns_at_2s = jb.stats().underruns;
            }
            if ms > 2000 {
                for i in (0..out.len()).step_by(2) {
                    max_jump = max_jump.max((out[i] as i64 - prev as i64).abs());
                    prev = out[i];
                }
            } else {
                prev = out[out.len() - 2];
            }
        }
        SimResult {
            max_jump,
            steady_underruns: jb.stats().underruns - underruns_at_2s,
        }
    }

    /// Port of `TestSteadyState`.
    #[test]
    fn steady_state() {
        let mut jb = JitterBuffer::new(48000, 2, 15, 150);
        let r = sim(&mut jb, 10, 0, 0);
        let st = jb.stats();
        assert!(
            r.steady_underruns == 0
                && st.target_ms == 15
                && st.level_ms <= 35
                && r.max_jump <= SMOOTH_JUMP,
            "bad steady state: {st:?} under={} jump={}",
            r.steady_underruns,
            r.max_jump
        );
    }

    /// Port of `TestAdaptsToStalls`: like AWDL, a 100 ms stall every 1 s.
    /// Once the buffer has adapted there must be neither underruns nor clicks.
    #[test]
    fn adapts_to_stalls() {
        let mut jb = JitterBuffer::new(48000, 2, 15, 150);
        let r = sim(&mut jb, 20, 1000, 100);
        let st = jb.stats();
        assert_eq!(r.steady_underruns, 0, "underruns after adapting: {st:?}");
        assert!(
            (100..=140).contains(&st.target_ms),
            "wrong target: {}ms",
            st.target_ms
        );
        assert!(r.max_jump <= SMOOTH_JUMP, "click/jump: {}", r.max_jump);
    }

    /// Port of `TestTrimIsSmooth`: cutting a big burst must not cause a sharp
    /// jump, thanks to the crossfade.
    #[test]
    fn trim_is_smooth() {
        let mut jb = JitterBuffer::new(48000, 2, 15, 150);
        let mut inp = vec![0i16; 480 * 2];
        let t0 = Instant::now();
        for i in 0..40 {
            // 400 ms at once
            for f in 0..480 {
                let v = (AMP * (2.0 * PI * 440.0 * (i * 480 + f) as f64 / RATE).sin()) as i16;
                inp[2 * f] = v;
                inp[2 * f + 1] = v;
            }
            jb.push(&inp, t0, false);
        }
        let mut out = vec![0i16; 240 * 2];
        jb.pull(&mut out, t0);
        let st = jb.stats();
        assert!(
            st.trimmed == 1 && st.level_ms <= 40,
            "burst not trimmed: {st:?}"
        );
        for i in (2..out.len()).step_by(2) {
            // During the crossfade two sine phases mix — still no sharp jump.
            let d = (out[i] as i64 - out[i - 2] as i64).abs();
            assert!(d <= 2 * SMOOTH_JUMP, "jump {d} in crossfade (i={i})");
        }
    }

    /// Port of `TestQuietSkip`: excess buffer is dropped quickly (up to 2×)
    /// in silence — without changing the music's speed.
    #[test]
    fn quiet_skip() {
        let mut jb = JitterBuffer::new(48000, 2, 15, 150);
        let t0 = Instant::now();
        jb.push(&vec![0i16; 100 * 48 * 2], t0, false); // 100 ms of silence
        let mut out = vec![0i16; 240 * 2];
        for _ in 0..10 {
            // play 50 ms
            jb.pull(&mut out, t0);
        }
        let st = jb.stats();
        assert!(
            st.quiet_skips >= 1500 && st.level_ms <= 15,
            "not skipped in silence: {st:?}"
        );
    }

    /// Streams a continuous sine where the producer runs `1 + drift` times
    /// the consumer's speed, for `secs` of virtual time. At `disc_at_ms`
    /// (if any) 7 ms of audio (a phase jump) is "lost" and the next chunk is
    /// pushed with `disc = disc_flag`. Returns per-5 ms snapshots after 2 s
    /// and the max output jump after 2 s.
    fn drift_sim(
        jb: &mut JitterBuffer,
        secs: u64,
        drift: f64,
        disc_at_ms: Option<u64>,
        disc_flag: bool,
    ) -> (Vec<JbStats>, i64) {
        let t0 = Instant::now();
        let mut phase = 0.0f64;
        let mut acc = 0.0f64;
        let mut out = vec![0i16; 240 * 2];
        let mut prev = 0i16;
        let mut max_jump = 0i64;
        let mut snaps = Vec::new();
        let mut chunk = Vec::with_capacity(500 * 2);
        for ms in (0..secs * 1000).step_by(5) {
            let now = t0 + Duration::from_millis(ms);
            if ms % 10 == 0 {
                acc += 480.0 * (1.0 + drift);
                let n = acc as usize;
                acc -= n as f64;
                let disc = disc_at_ms == Some(ms);
                if disc {
                    phase += 2.0 * PI * 440.0 / RATE * 7.0 * 48.0;
                }
                chunk.clear();
                for _ in 0..n {
                    let v = (AMP * phase.sin()) as i16;
                    phase += 2.0 * PI * 440.0 / RATE;
                    chunk.extend_from_slice(&[v, v]);
                }
                jb.push(&chunk, now, disc && disc_flag);
            }
            jb.pull(&mut out, now);
            if ms > 2000 {
                for i in (0..out.len()).step_by(2) {
                    max_jump = max_jump.max((out[i] as i64 - prev as i64).abs());
                    prev = out[i];
                }
                snaps.push(jb.stats());
            } else {
                prev = out[out.len() - 2];
            }
        }
        (snaps, max_jump)
    }

    /// A chunk pushed with `disc = true` after lost audio is spliced with a
    /// fade-out/fade-in: no click at the seam. Without the flag the same
    /// phase jump does click (so the test really exercises the fade).
    #[test]
    fn disc_push_fades_seam() {
        let mut jb = JitterBuffer::new(48000, 2, 15, 150);
        let (_, jump) = drift_sim(&mut jb, 5, 0.0, Some(3000), true);
        assert!(jump <= SMOOTH_JUMP, "click at disc seam: {jump}");

        let mut jb = JitterBuffer::new(48000, 2, 15, 150);
        let (_, jump) = drift_sim(&mut jb, 5, 0.0, Some(3000), false);
        assert!(
            jump > 2 * SMOOTH_JUMP,
            "control: unfaded seam should click: {jump}"
        );
    }

    /// Producer clock 0.1 % faster than the consumer for 60 s: the surplus
    /// (60 ms over the run) is absorbed by the speed-up (sine is never quiet,
    /// so no quiet skips). The level stays bounded near the target (the
    /// speed-up only engages once the smoothed level exceeds target + 10 ms),
    /// the average speed matches the drift, no underruns and no trims.
    #[test]
    fn absorbs_faster_producer() {
        let mut jb = JitterBuffer::new(48000, 2, 15, 150);
        let (snaps, jump) = drift_sim(&mut jb, 60, 0.001, None, false);
        let st = jb.stats();
        let max_level = snaps.iter().map(|s| s.level_ms).max().unwrap();
        let max_speed = snaps.iter().map(|s| s.speed_pct).fold(f64::MIN, f64::max);
        let tail = &snaps[snaps.len() / 2..];
        let mean_speed = tail.iter().map(|s| s.speed_pct).sum::<f64>() / tail.len() as f64;
        assert_eq!(st.underruns, 0);
        assert_eq!(st.trimmed, 0);
        assert_eq!(st.quiet_skips, 0);
        assert_eq!(st.target_ms, 15);
        assert!(
            max_level <= st.target_ms + 30,
            "level not bounded: {max_level}"
        );
        assert!(max_speed > 0.0, "never sped up");
        assert!(
            (0.05..=0.2).contains(&mean_speed),
            "mean speed {mean_speed}% does not track the 0.1% drift"
        );
        assert!(jump <= SMOOTH_JUMP, "click: {jump}");
    }

    /// Producer clock 0.1 % slower for 60 s.
    ///
    /// The slow-down (−0.3 % max) engages as soon as the smoothed level is
    /// below the target (Linker deviation — audiobridge waited for target/2,
    /// which let the level sag to ~2 ms here and underrun with smaller
    /// buffers). Guaranteed (and asserted): the level is held near the
    /// target with a wide margin from empty, no underruns, no trims, no quiet
    /// skips (a sine is never quiet), still playing, speed tracks −0.1 % on
    /// average, no clicks.
    #[test]
    fn absorbs_slower_producer() {
        let mut jb = JitterBuffer::new(48000, 2, 15, 150);
        let (snaps, jump) = drift_sim(&mut jb, 60, -0.001, None, false);
        let st = jb.stats();
        let min_level = snaps.iter().map(|s| s.level_ms).min().unwrap();
        let min_speed = snaps.iter().map(|s| s.speed_pct).fold(f64::MAX, f64::min);
        let tail = &snaps[snaps.len() / 2..];
        let mean_speed = tail.iter().map(|s| s.speed_pct).sum::<f64>() / tail.len() as f64;
        assert_eq!(st.underruns, 0, "{st:?}");
        assert_eq!(st.trimmed, 0);
        assert_eq!(st.quiet_skips, 0);
        assert_eq!(st.target_ms, 15);
        assert!(st.playing && snaps.iter().all(|s| s.playing));
        assert!(min_level >= 5, "level came close to empty: {min_level}");
        assert!(min_speed < 0.0, "never slowed down");
        assert!(
            (-0.2..=-0.05).contains(&mean_speed),
            "mean speed {mean_speed}% does not track the -0.1% drift"
        );
        // Level is held around the target (15 ms).
        let mean_avg = tail.iter().map(|s| s.avg_ms as f64).sum::<f64>() / tail.len() as f64;
        assert!((12.0..=20.0).contains(&mean_avg), "avg level {mean_avg} ms");
        assert!(jump <= SMOOTH_JUMP, "click: {jump}");
    }
}
