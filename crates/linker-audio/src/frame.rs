//! PCM framing helpers — port of audiobridge `internal/codec/frame.go`.
//!
//! * [`Resampler`]: linear resampling of interleaved stereo `i16` from any
//!   sample rate to 48 kHz. Opus only accepts 48 kHz, while some devices
//!   (Windows, some USB headsets) run at 44.1 kHz.
//! * [`Framer`]: collects arbitrary-length PCM chunks (whatever the audio
//!   callback hands us) into frames of exactly `size` samples per channel,
//!   the size the Opus encoder wants.
//!
//! Plus small, allocation-free channel conversion helpers used by the
//! platform backends (cpal / PipeWire), and the peak meter of the Go sender.

use crate::{CHANNELS, RATE};

/// Linear resampler, any rate → 48 kHz, interleaved stereo `i16`.
#[derive(Debug, Clone)]
pub struct Resampler {
    step: f64,
    /// Read position in input frames: 0 = the previous chunk's last frame
    /// (`prev`), 1 = `input[0]`, …
    pos: f64,
    prev: [i16; CHANNELS],
    passthrough: bool,
}

impl Resampler {
    /// Resampler from `from_rate` Hz to 48 kHz. At 48 kHz it is an exact
    /// pass-through (see [`Resampler::is_passthrough`]).
    ///
    /// # Panics
    /// If `from_rate` is 0. (Go's `NewResampler(0)` builds a resampler whose
    /// `Process` never advances and loops forever; failing loudly at
    /// construction is better than hanging the audio thread.)
    pub fn new(from_rate: u32) -> Resampler {
        assert!(from_rate > 0, "Resampler: input sample rate must be > 0");
        Resampler {
            step: from_rate as f64 / RATE as f64,
            pos: 0.0,
            prev: [0; CHANNELS],
            passthrough: from_rate == RATE,
        }
    }

    /// True when the input is already 48 kHz and [`process`](Self::process)
    /// just copies.
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Resample `input` and append the result to `out`. State (fractional
    /// position and the last input frame) carries over between calls so
    /// chunk boundaries are seamless.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        let n = input.len() / CHANNELS;
        if n == 0 {
            return;
        }
        let prev = self.prev;
        let frame = |i: usize| -> (i16, i16) {
            if i == 0 {
                (prev[0], prev[1])
            } else {
                (input[(i - 1) * CHANNELS], input[(i - 1) * CHANNELS + 1])
            }
        };
        out.reserve(((n as f64 / self.step) as usize + 1) * CHANNELS);
        while self.pos < n as f64 {
            let i = self.pos as usize;
            // Same float32 interpolation as Go (`int16(float32)` truncates
            // toward zero, as does Rust `as`).
            let f = (self.pos - i as f64) as f32;
            let (l0, r0) = frame(i);
            let (l1, r1) = frame(i + 1);
            out.push((l0 as f32 + (l1 as f32 - l0 as f32) * f) as i16);
            out.push((r0 as f32 + (r1 as f32 - r0 as f32) * f) as i16);
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev = [input[(n - 1) * CHANNELS], input[(n - 1) * CHANNELS + 1]];
    }
}

/// Linear *pull* resampler, 48 kHz → `to_rate`, interleaved stereo `i16`.
///
/// The playback counterpart of [`Resampler`]: a device callback asks for
/// exactly `N` frames at its own rate, and [`fill`](Self::fill) pulls just
/// enough 48 kHz frames from the source to interpolate them. The fractional
/// read position and the not-yet-passed input frames carry over between
/// calls, so callback boundaries are seamless (no clicks) and over time
/// exactly `to_rate`/48000 of the source rate is consumed.
///
/// Real-time safe after warm-up: the internal buffer grows only when a
/// callback is larger than every previous one.
#[derive(Debug, Clone)]
pub struct PullResampler {
    /// Input (48 kHz) frames per output frame.
    step: f64,
    /// Read position in frames relative to `buf[0]`.
    pos: f64,
    /// Interleaved stereo input frames: the carried-over history followed
    /// by the frames pulled in the current call.
    buf: Vec<i16>,
    passthrough: bool,
}

impl PullResampler {
    /// Resampler producing `to_rate` Hz from a 48 kHz source. At 48 kHz
    /// [`fill`](Self::fill) is an exact pass-through.
    ///
    /// # Panics
    /// If `to_rate` is 0.
    pub fn new(to_rate: u32) -> PullResampler {
        assert!(to_rate > 0, "PullResampler: output sample rate must be > 0");
        let mut buf = Vec::with_capacity(1024 * CHANNELS);
        // Start from one silent history frame: the first output frame is
        // interpolated between silence and the first source frame.
        buf.extend_from_slice(&[0; CHANNELS]);
        PullResampler {
            step: RATE as f64 / to_rate as f64,
            pos: 0.0,
            buf,
            passthrough: to_rate == RATE,
        }
    }

    /// True when the output rate is 48 kHz and [`fill`](Self::fill) just
    /// forwards to the source.
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Fill `out` completely (interleaved stereo at `to_rate`). `pull` is
    /// asked (at most once per call) to fill a slice of 48 kHz interleaved
    /// stereo frames completely.
    pub fn fill(&mut self, out: &mut [i16], mut pull: impl FnMut(&mut [i16])) {
        if self.passthrough {
            pull(out);
            return;
        }
        let n = out.len() / CHANNELS;
        if n == 0 {
            return;
        }
        // Positions used: pos + k*step for k in 0..n; each interpolates
        // between frames floor(p) and floor(p)+1.
        let last = self.pos + (n - 1) as f64 * self.step;
        let need = last as usize + 2;
        let have = self.buf.len() / CHANNELS;
        if need > have {
            // Grows (allocates) only for a callback larger than any before.
            self.buf.resize(need * CHANNELS, 0);
            pull(&mut self.buf[have * CHANNELS..]);
        }
        let buf = &self.buf;
        let mut p = self.pos;
        for o in out.as_chunks_mut::<CHANNELS>().0 {
            let i = p as usize;
            let f = (p - i as f64) as f32;
            let a = &buf[i * CHANNELS..];
            let (l0, r0, l1, r1) = (a[0], a[1], a[2], a[3]);
            o[0] = (l0 as f32 + (l1 as f32 - l0 as f32) * f) as i16;
            o[1] = (r0 as f32 + (r1 as f32 - r0 as f32) * f) as i16;
            p += self.step;
        }
        // Drop the frames no future position can reach (keep at least one).
        let total = self.buf.len() / CHANNELS;
        let drop = (p as usize).min(total - 1);
        self.buf.copy_within(drop * CHANNELS.., 0);
        self.buf.truncate((total - drop) * CHANNELS);
        self.pos = p - drop as f64;
    }
}

/// Collects PCM chunks of any length into frames of exactly `size` samples
/// per channel.
#[derive(Debug, Clone)]
pub struct Framer {
    size: usize,
    buf: Vec<i16>,
}

impl Framer {
    /// `size_frames` is per channel (e.g. 240 for 5 ms at 48 kHz).
    pub fn new(size_frames: usize) -> Framer {
        Framer {
            size: size_frames,
            buf: Vec::with_capacity(size_frames * CHANNELS * 2),
        }
    }

    /// Frame size in samples per channel.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Append `pcm`; `f` is called for every complete frame
    /// (`size * CHANNELS` interleaved samples). The remainder is kept.
    pub fn push(&mut self, pcm: &[i16], mut f: impl FnMut(&[i16])) {
        let fs = self.size * CHANNELS;
        self.buf.extend_from_slice(pcm);
        if fs == 0 {
            return;
        }
        let mut off = 0;
        while self.buf.len() - off >= fs {
            f(&self.buf[off..off + fs]);
            off += fs;
        }
        self.buf.drain(..off);
    }

    /// Buffered frames (per channel) not yet emitted.
    pub fn pending_frames(&self) -> usize {
        self.buf.len() / CHANNELS
    }

    /// Drop the buffered remainder (e.g. on stream restart).
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

#[inline]
fn f32_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

#[inline]
fn i16_to_f32(v: i16) -> f32 {
    v as f32 / 32768.0
}

/// Shared stereo down/up-mix rule: returns (L, R) of one input frame.
#[inline]
fn pick_lr<T: Copy>(fr: &[T]) -> (T, T) {
    if fr.len() == 1 {
        (fr[0], fr[0])
    } else {
        (fr[0], fr[1])
    }
}

/// Interleaved `f32` with `channels` channels → stereo `i16`, appended to
/// `out`. Mono is duplicated, stereo copied, more channels keep the first
/// two (FL, FR). Samples are clamped to `[-1, 1]`.
pub fn to_stereo_i16_f32(input: &[f32], channels: usize, out: &mut Vec<i16>) {
    if channels == 0 {
        return;
    }
    out.reserve(input.len() / channels * CHANNELS);
    for fr in input.chunks_exact(channels) {
        let (l, r) = pick_lr(fr);
        out.push(f32_to_i16(l));
        out.push(f32_to_i16(r));
    }
}

/// Interleaved `i16` with `channels` channels → stereo `i16`, appended to
/// `out` (same channel rule as [`to_stereo_i16_f32`]).
pub fn to_stereo_i16_i16(input: &[i16], channels: usize, out: &mut Vec<i16>) {
    if channels == 0 {
        return;
    }
    if channels == CHANNELS {
        // Whole frames only, like the other paths: a stray trailing sample
        // would swap L/R for the rest of the stream.
        out.extend_from_slice(&input[..input.len() / CHANNELS * CHANNELS]);
        return;
    }
    out.reserve(input.len() / channels * CHANNELS);
    for fr in input.chunks_exact(channels) {
        let (l, r) = pick_lr(fr);
        out.push(l);
        out.push(r);
    }
}

/// Stereo `i16` → interleaved `f32` with `channels` channels, written to
/// `out` (`out.len() == input.len() / 2 * channels`). Mono gets the average
/// of L and R, stereo is copied, more channels get L, R and silence.
pub fn from_stereo_i16_f32(input: &[i16], out: &mut [f32], channels: usize) {
    if channels == 0 {
        return;
    }
    debug_assert_eq!(out.len(), input.len() / CHANNELS * channels);
    for (src, dst) in input
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .zip(out.chunks_exact_mut(channels))
    {
        if channels == 1 {
            dst[0] = ((src[0] as i32 + src[1] as i32) as f32 / 2.0) / 32768.0;
        } else {
            dst[0] = i16_to_f32(src[0]);
            dst[1] = i16_to_f32(src[1]);
            dst[2..].fill(0.0);
        }
    }
}

/// Stereo `i16` → interleaved `i16` with `channels` channels (same rule as
/// [`from_stereo_i16_f32`]; the mono average rounds toward zero).
pub fn from_stereo_i16_i16(input: &[i16], out: &mut [i16], channels: usize) {
    if channels == 0 {
        return;
    }
    debug_assert_eq!(out.len(), input.len() / CHANNELS * channels);
    if channels == CHANNELS {
        let n = out.len().min(input.len());
        out[..n].copy_from_slice(&input[..n]);
        return;
    }
    for (src, dst) in input
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .zip(out.chunks_exact_mut(channels))
    {
        if channels == 1 {
            dst[0] = ((src[0] as i32 + src[1] as i32) / 2) as i16;
        } else {
            dst[0] = src[0];
            dst[1] = src[1];
            dst[2..].fill(0);
        }
    }
}

/// Peak absolute sample value. `u16` so that `|-32768| = 32768` fits.
pub fn peak(pcm: &[i16]) -> u16 {
    pcm.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0)
}

/// Peak as dBFS text, like the Go sender's `dbfs` (`"-inf dBFS"` for 0,
/// full scale = 32768).
pub fn dbfs(peak: u16) -> String {
    if peak == 0 {
        return "-inf dBFS".to_string();
    }
    format!("{:.1} dBFS", 20.0 * (peak as f64 / 32768.0).log10())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of `TestResample44k`: 44.1 kHz → 48 kHz gives the right output
    /// length and the sine frequency is preserved.
    #[test]
    fn resample_44k() {
        const IN: u32 = 44_100;
        let mut rs = Resampler::new(IN);
        assert!(!rs.is_passthrough());
        let mut out = Vec::new();
        let mut chunk = vec![0i16; 441 * CHANNELS];
        let mut ph = 0.0f64;
        for _ in 0..100 {
            // 1 s in 10 ms chunks
            for i in 0..441 {
                let v = (10000.0 * ph.sin()) as i16;
                ph += 2.0 * std::f64::consts::PI * 1000.0 / IN as f64;
                chunk[2 * i] = v;
                chunk[2 * i + 1] = v;
            }
            rs.process(&chunk, &mut out);
        }
        let n = out.len() / CHANNELS;
        assert!(
            (47990..=48010).contains(&n),
            "{n} frames after 1 s, ~48000 expected"
        );
        // zero crossings ≈ 2 * 1000
        let mut zc = 0;
        for i in (2..out.len()).step_by(2) {
            if (out[i - 2] < 0) != (out[i] < 0) {
                zc += 1;
            }
        }
        assert!(
            (1990..=2010).contains(&zc),
            "frequency broken: {zc} zero crossings"
        );
    }

    #[test]
    fn resample_48k_is_exact_copy() {
        let mut rs = Resampler::new(48_000);
        assert!(rs.is_passthrough());
        let input: Vec<i16> = (0..960).map(|i| (i * 31 - 9000) as i16).collect();
        let mut out = vec![1, 2];
        rs.process(&input, &mut out);
        assert_eq!(&out[..2], &[1, 2]);
        assert_eq!(&out[2..], &input[..]);
    }

    /// Pull `secs` seconds of output at `to_rate` from a 48 kHz sine source
    /// in callbacks of the given (cycling) sizes; returns (output, pulled
    /// 48 kHz frames).
    fn pull_sine(to_rate: u32, sizes: &[usize], frames: usize, hz: f64) -> (Vec<i16>, usize) {
        let mut rs = PullResampler::new(to_rate);
        let mut out = vec![0i16; frames * CHANNELS];
        let mut ph = 0.0f64;
        let mut pulled = 0usize;
        let mut off = 0;
        let mut k = 0;
        while off < frames {
            let n = sizes[k % sizes.len()].min(frames - off);
            k += 1;
            rs.fill(&mut out[off * CHANNELS..(off + n) * CHANNELS], |buf| {
                for fr in buf.as_chunks_mut::<CHANNELS>().0 {
                    let v = (10000.0 * ph.sin()) as i16;
                    ph += 2.0 * std::f64::consts::PI * hz / RATE as f64;
                    fr[0] = v;
                    fr[1] = v;
                }
                pulled += buf.len() / CHANNELS;
            });
            off += n;
        }
        (out, pulled)
    }

    fn zero_crossings(pcm: &[i16]) -> usize {
        let mut zc = 0;
        for i in (2..pcm.len()).step_by(2) {
            if (pcm[i - 2] < 0) != (pcm[i] < 0) {
                zc += 1;
            }
        }
        zc
    }

    /// 1 s of device output consumes 1 s (±2 frames) of 48 kHz source.
    #[test]
    fn pull_resample_length_exact() {
        for rate in [44_100u32, 96_000] {
            let (out, pulled) = pull_sine(rate, &[128], rate as usize, 1000.0);
            assert_eq!(out.len(), rate as usize * CHANNELS);
            assert!(
                (47_998..=48_002).contains(&pulled),
                "{rate} Hz: pulled {pulled} frames for 1 s, 48000 expected"
            );
        }
    }

    /// Like `resample_44k`: a 1 kHz sine keeps its frequency.
    #[test]
    fn pull_resample_preserves_frequency() {
        for rate in [44_100u32, 96_000] {
            let (out, _) = pull_sine(rate, &[441, 128, 512], rate as usize, 1000.0);
            let zc = zero_crossings(&out);
            assert!(
                (1990..=2010).contains(&zc),
                "{rate} Hz: frequency broken: {zc} zero crossings"
            );
        }
    }

    #[test]
    fn pull_resample_48k_is_exact_passthrough() {
        let mut rs = PullResampler::new(48_000);
        assert!(rs.is_passthrough());
        let src: Vec<i16> = (0..960).map(|i| (i * 31 - 9000) as i16).collect();
        let mut out = vec![0i16; 960];
        let mut calls = 0;
        rs.fill(&mut out, |buf| {
            calls += 1;
            assert_eq!(buf.len(), src.len());
            buf.copy_from_slice(&src);
        });
        assert_eq!(calls, 1);
        assert_eq!(out, src);
    }

    /// Odd callback sizes give the same signal as one huge callback, and a
    /// low sine never jumps more than its natural slope (no clicks at the
    /// callback boundaries).
    #[test]
    fn pull_resample_seamless_across_callbacks() {
        for rate in [44_100u32, 96_000, 22_050] {
            let frames = rate as usize / 2;
            let (whole, _) = pull_sine(rate, &[frames], frames, 440.0);
            let (chunked, _) = pull_sine(rate, &[1, 127, 61, 333, 7, 89, 2], frames, 440.0);
            for (i, (a, b)) in whole.iter().zip(&chunked).enumerate() {
                assert!(
                    (*a as i32 - *b as i32).abs() <= 1,
                    "{rate} Hz: sample {i}: {a} vs {b}"
                );
            }
            // max slope of 10000*sin(2π·440·t) per output sample, + rounding
            let max_step = 10000.0 * 2.0 * std::f64::consts::PI * 440.0 / rate as f64 + 2.0;
            for i in (2..chunked.len()).step_by(2) {
                let d = (chunked[i] as f64 - chunked[i - 2] as f64).abs();
                assert!(d <= max_step, "{rate} Hz: jump {d} at frame {}", i / 2);
            }
        }
    }

    #[test]
    #[should_panic(expected = "sample rate must be > 0")]
    fn pull_resampler_rejects_zero_rate() {
        let _ = PullResampler::new(0);
    }

    /// Port of `TestFramer`.
    #[test]
    fn framer() {
        let mut f = Framer::new(480);
        assert_eq!(f.size(), 480);
        let mut frames = 0;
        for _ in 0..10 {
            f.push(&[0i16; 441 * CHANNELS], |fr| {
                assert_eq!(fr.len(), 480 * CHANNELS, "frame length");
                frames += 1;
            });
        }
        assert_eq!(frames, 9); // 4410 / 480 = 9
        assert_eq!(f.pending_frames(), 4410 - 9 * 480);
        f.clear();
        assert_eq!(f.pending_frames(), 0);
    }

    #[test]
    fn framer_preserves_sample_order() {
        let mut f = Framer::new(3);
        let mut got = Vec::new();
        let src: Vec<i16> = (0..40).collect();
        for c in src.chunks(7) {
            f.push(c, |fr| got.extend_from_slice(fr));
        }
        assert_eq!(got, (0..36).collect::<Vec<i16>>());
        assert_eq!(f.pending_frames(), 2);
    }

    #[test]
    fn to_stereo_conversions() {
        let mut out = Vec::new();
        to_stereo_i16_f32(&[0.5, -2.0], 1, &mut out);
        assert_eq!(out, [16383, 16383, -32767, -32767]);
        out.clear();
        to_stereo_i16_f32(&[1.0, -1.0, 0.25, 0.0], 2, &mut out);
        assert_eq!(out, [32767, -32767, 8191, 0]);
        out.clear();
        to_stereo_i16_f32(&[0.5, -0.5, 0.9, 0.9, 0.9, 0.9], 6, &mut out);
        assert_eq!(out, [16383, -16383]);

        let mut out = vec![9];
        to_stereo_i16_i16(&[5, -6], 1, &mut out);
        assert_eq!(out, [9, 5, 5, -6, -6]);
        out.clear();
        to_stereo_i16_i16(&[1, 2, 3, 4], 2, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
        // A trailing partial frame is dropped on every path, so L/R never
        // swap (regression: the stereo fast path used to copy it).
        out.clear();
        to_stereo_i16_i16(&[1, 2, 3], 2, &mut out);
        assert_eq!(out, [1, 2]);
        out.clear();
        to_stereo_i16_f32(&[0.5, 0.5, 0.5], 2, &mut out);
        assert_eq!(out, [16383, 16383]);
        out.clear();
        to_stereo_i16_i16(&[1, 2, 3, 4, 5, 6, 7, 8], 4, &mut out);
        assert_eq!(out, [1, 2, 5, 6]);
    }

    #[test]
    fn from_stereo_conversions() {
        let st = [16384i16, -16384, 100, 301];
        let mut m = [0f32; 2];
        from_stereo_i16_f32(&st, &mut m, 1);
        assert_eq!(m[0], 0.0);
        assert!((m[1] - 200.5 / 32768.0).abs() < 1e-7);
        let mut s = [0f32; 4];
        from_stereo_i16_f32(&st, &mut s, 2);
        assert_eq!(s[..2], [0.5, -0.5]);
        let mut q = [9f32; 8];
        from_stereo_i16_f32(&st, &mut q, 4);
        assert_eq!(q[..4], [0.5, -0.5, 0.0, 0.0]);

        let mut m = [0i16; 2];
        from_stereo_i16_i16(&st, &mut m, 1);
        assert_eq!(m, [0, 200]);
        let mut s = [0i16; 4];
        from_stereo_i16_i16(&st, &mut s, 2);
        assert_eq!(s, st);
        let mut q = [9i16; 6];
        from_stereo_i16_i16(&st, &mut q, 3);
        assert_eq!(q, [16384, -16384, 0, 100, 301, 0]);
    }

    /// Rate 0 would make `process` loop forever (as in Go); reject it early.
    #[test]
    #[should_panic(expected = "sample rate must be > 0")]
    fn resampler_rejects_zero_rate() {
        let _ = Resampler::new(0);
    }

    #[test]
    fn peak_and_dbfs() {
        assert_eq!(peak(&[]), 0);
        assert_eq!(peak(&[3, -7, 5]), 7);
        assert_eq!(peak(&[i16::MIN, 1]), 32768);
        assert_eq!(dbfs(0), "-inf dBFS");
        assert_eq!(dbfs(32768), "0.0 dBFS");
        assert_eq!(dbfs(16384), "-6.0 dBFS");
    }
}
