//! Opus codec — port of audiobridge `internal/codec/codec.go`.
//!
//! A thin layer over libopus (vendored, via the `opus` crate): 48 kHz stereo,
//! 2.5/5/10/20 ms frames, `RESTRICTED_LOWDELAY` mode (CELT only, algorithmic
//! delay = frame + 2.5 ms). A lost frame is replaced with `decode(None)` —
//! libopus packet-loss concealment synthesises plausible audio.

use crate::{CHANNELS, RATE};

/// Largest frame the decoder may produce per channel (20 ms).
pub const MAX_FRAME_SIZE: usize = RATE as usize / 1000 * 20;
/// Largest possible Opus frame in bytes.
pub const MAX_PACKET: usize = 1275;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame duration must be 2.5, 5, 10 or 20 ms, not {0}")]
    FrameMs(f64),
    #[error("expected {expected} samples, got {got}")]
    FrameLen { expected: usize, got: usize },
    #[error("opus: {0}")]
    Opus(#[from] opus::Error),
}

/// Samples per channel in one frame of `ms` (2.5, 5, 10, 20).
pub fn frame_size(ms: f64) -> Result<usize, CodecError> {
    if [2.5, 5.0, 10.0, 20.0].contains(&ms) {
        Ok((RATE as f64 * ms / 1000.0) as usize)
    } else {
        Err(CodecError::FrameMs(ms))
    }
}

pub struct Encoder {
    e: opus::Encoder,
    frame_size: usize,
    bitrate: i32,
    buf: Box<[u8; MAX_PACKET]>,
}

impl Encoder {
    /// `bitrate` in bit/s (e.g. 128_000), `complexity` 0..=10, `frame_ms`
    /// 2.5/5/10/20.
    pub fn new(bitrate: i32, complexity: i32, frame_ms: f64) -> Result<Encoder, CodecError> {
        let frame_size = frame_size(frame_ms)?;
        let mut e = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::LowDelay)?;
        e.set_bitrate(opus::Bitrate::Bits(bitrate))?;
        e.set_complexity(complexity)?;
        Ok(Encoder {
            e,
            frame_size,
            bitrate,
            buf: Box::new([0; MAX_PACKET]),
        })
    }

    /// Samples per channel in a frame.
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    pub fn bitrate(&self) -> i32 {
        self.bitrate
    }

    pub fn set_bitrate(&mut self, bitrate: i32) -> Result<(), CodecError> {
        self.e.set_bitrate(opus::Bitrate::Bits(bitrate))?;
        self.bitrate = bitrate;
        Ok(())
    }

    /// Is the encoder really in `RESTRICTED_LOWDELAY` mode?
    pub fn is_low_delay(&mut self) -> bool {
        matches!(self.e.get_application(), Ok(opus::Application::LowDelay))
    }

    /// Algorithmic look-ahead in samples per channel.
    pub fn lookahead(&mut self) -> usize {
        self.e.get_lookahead().unwrap_or(0).max(0) as usize
    }

    /// Takes exactly `frame_size() * CHANNELS` samples. The result is valid
    /// until the next call.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<&[u8], CodecError> {
        self.encode_max(pcm, MAX_PACKET)
    }

    /// Like [`encode`](Self::encode) but the frame never exceeds `max_bytes`
    /// (libopus lowers this frame's bitrate to fit — used to respect the
    /// link's datagram size).
    pub fn encode_max(&mut self, pcm: &[i16], max_bytes: usize) -> Result<&[u8], CodecError> {
        let expected = self.frame_size * CHANNELS;
        if pcm.len() != expected {
            return Err(CodecError::FrameLen {
                expected,
                got: pcm.len(),
            });
        }
        let cap = max_bytes.clamp(3, MAX_PACKET);
        let n = self.e.encode(pcm, &mut self.buf[..cap])?;
        Ok(&self.buf[..n])
    }
}

pub struct Decoder {
    d: opus::Decoder,
    /// Length of the last decoded frame per channel — PLC produces this much.
    last: usize,
}

impl Decoder {
    pub fn new() -> Result<Decoder, CodecError> {
        Ok(Decoder {
            d: opus::Decoder::new(RATE, opus::Channels::Stereo)?,
            last: RATE as usize / 100,
        })
    }

    /// `pkt = None` → PLC (concealment for a lost frame).
    /// `out.len() >= MAX_FRAME_SIZE * CHANNELS`. Returns samples written
    /// (all channels).
    pub fn decode(&mut self, pkt: Option<&[u8]>, out: &mut [i16]) -> Result<usize, CodecError> {
        match pkt {
            None => {
                let n = self
                    .d
                    .decode(&[], &mut out[..self.last * CHANNELS], false)?;
                Ok(n * CHANNELS)
            }
            Some(p) => {
                let max = MAX_FRAME_SIZE.min(out.len() / CHANNELS);
                let n = self.d.decode(p, &mut out[..max * CHANNELS], false)?;
                if n > 0 {
                    self.last = n;
                }
                Ok(n * CHANNELS)
            }
        }
    }

    /// Frame length (per channel) PLC will generate.
    pub fn last_frame_size(&self) -> usize {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn signal(n: usize) -> Vec<i16> {
        let mut s = vec![0i16; n * CHANNELS];
        for i in 0..n {
            let t = i as f64 / RATE as f64;
            let v = 6000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
                + 3000.0 * (2.0 * std::f64::consts::PI * 1234.0 * t).sin()
                + 1500.0 * (2.0 * std::f64::consts::PI * 5100.0 * t).sin();
            s[2 * i] = v as i16;
            s[2 * i + 1] = (v * 0.8) as i16;
        }
        s
    }

    /// Port of `TestRoundTrip`: encode+decode must be far faster than real
    /// time and preserve the signal (SNR ≥ 20 dB after aligning the codec delay).
    #[test]
    fn round_trip() {
        for ms in [10.0, 5.0, 2.5] {
            round_trip_ms(ms);
        }
    }

    fn round_trip_ms(frame_ms: f64) {
        let mut enc = Encoder::new(256_000, 8, frame_ms).unwrap();
        assert!(enc.is_low_delay(), "RESTRICTED_LOWDELAY must be active");
        let fs = enc.frame_size();
        let mut dec = Decoder::new().unwrap();
        let frames = (3000.0 / frame_ms) as usize; // 3 s
        let input = signal(frames * fs);
        let mut out = Vec::with_capacity(input.len());
        let mut buf = vec![0i16; MAX_FRAME_SIZE * CHANNELS];
        let (mut enc_t, mut dec_t) = (Duration::ZERO, Duration::ZERO);
        let mut bytes = 0;
        for f in 0..frames {
            let t0 = Instant::now();
            let pkt = enc
                .encode(&input[f * fs * CHANNELS..(f + 1) * fs * CHANNELS])
                .unwrap()
                .to_vec();
            enc_t += t0.elapsed();
            bytes += pkt.len();
            let t0 = Instant::now();
            let n = dec.decode(Some(&pkt), &mut buf).unwrap();
            dec_t += t0.elapsed();
            out.extend_from_slice(&buf[..n]);
        }
        eprintln!(
            "{frame_ms}ms: encode {:.3}ms/frame, decode {:.3}ms/frame, avg packet {} bytes ({:.0} kbit/s)",
            enc_t.as_secs_f64() * 1000.0 / frames as f64,
            dec_t.as_secs_f64() * 1000.0 / frames as f64,
            bytes / frames,
            (bytes * 8) as f64 / 3.0 / 1000.0
        );
        let per = enc_t / frames as u32;
        assert!(
            per < Duration::from_secs_f64(frame_ms / 1000.0) / 3,
            "encode too slow: {per:?} per {frame_ms} ms frame"
        );

        // Find the codec delay, then measure SNR.
        let (mut best, mut best_d) = (-1.0f64, 0usize);
        for d in 0..400 {
            let (mut num, mut den) = (0.0, 0.0);
            for i in 48_000..96_000 {
                let (a, b) = (input[2 * i] as f64, out[2 * (i + d)] as f64);
                num += a * b;
                den += b * b;
            }
            let c = num / (den + 1.0).sqrt();
            if c > best {
                best = c;
                best_d = d;
            }
        }
        let (mut sig, mut noise) = (0.0, 0.0);
        for i in 48_000..96_000 {
            let (a, b) = (input[2 * i] as f64, out[2 * (i + best_d)] as f64);
            sig += a * a;
            noise += (a - b) * (a - b);
        }
        let snr = 10.0 * (sig / noise).log10();
        eprintln!(
            "{frame_ms}ms: codec delay {best_d} samples ({:.1} ms), SNR {snr:.1} dB",
            best_d as f64 / 48.0
        );
        assert!(snr >= 20.0, "quality too low: SNR {snr:.1} dB");
        // Low-delay mode: algorithmic delay is 2.5 ms (120 samples).
        assert!(best_d <= 130, "codec delay {best_d} samples too large");
    }

    /// Port of `TestPLC`: with no packet a full frame comes back and it is
    /// not silence (the sound continues).
    #[test]
    fn plc() {
        let mut enc = Encoder::new(256_000, 8, 5.0).unwrap();
        let mut dec = Decoder::new().unwrap();
        let fs = enc.frame_size();
        let input = signal(20 * fs);
        let mut buf = vec![0i16; MAX_FRAME_SIZE * CHANNELS];
        for f in 0..19 {
            let pkt = enc
                .encode(&input[f * fs * CHANNELS..(f + 1) * fs * CHANNELS])
                .unwrap()
                .to_vec();
            dec.decode(Some(&pkt), &mut buf).unwrap();
        }
        let n = dec.decode(None, &mut buf).unwrap();
        assert_eq!(n, fs * CHANNELS, "PLC length");
        let e: f64 = buf[..n].iter().map(|&v| v as f64 * v as f64).sum();
        let rms = (e / n as f64).sqrt();
        assert!(rms >= 500.0, "PLC returned silence (rms {rms:.0})");
    }

    #[test]
    fn frame_sizes_and_errors() {
        assert_eq!(frame_size(2.5).unwrap(), 120);
        assert_eq!(frame_size(5.0).unwrap(), 240);
        assert_eq!(frame_size(10.0).unwrap(), 480);
        assert_eq!(frame_size(20.0).unwrap(), 960);
        assert!(frame_size(7.0).is_err());
        assert!(Encoder::new(128_000, 8, 3.0).is_err());
        let mut e = Encoder::new(128_000, 8, 5.0).unwrap();
        assert!(e.encode(&[0; 100]).is_err(), "wrong frame length rejected");
        e.set_bitrate(64_000).unwrap();
        assert_eq!(e.bitrate(), 64_000);
    }

    #[test]
    fn corrupt_packet_is_an_error_not_a_panic() {
        let mut d = Decoder::new().unwrap();
        let mut buf = vec![0i16; MAX_FRAME_SIZE * CHANNELS];
        // TOC byte claiming a code-3 packet with an absurd frame count.
        let r = d.decode(Some(&[0xFF, 0xFF, 0xFF]), &mut buf);
        assert!(r.is_err());
        // PLC still works afterwards.
        assert!(d.decode(None, &mut buf).is_ok());
    }
}
