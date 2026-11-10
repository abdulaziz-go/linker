//! Send path — port of audiobridge `cmd/sender/main.go` (`onPCM` / `send`):
//! captured 48 kHz stereo PCM is collected into exact Opus frames, encoded,
//! packed with the previous `redundancy` frames and wrapped in the Linker
//! envelope, ready to go out as one QUIC datagram per frame.
//!
//! Also contains the automatic bitrate controller (new in Linker: the
//! receiver reports quality once a second and the sender adapts).

use crate::codec::{CodecError, Encoder};
use crate::frame::{peak, Framer};
use crate::proto::{self, Packer};
use crate::{CHANNELS, RATE};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SenderParams {
    pub bitrate_bps: i32,
    pub complexity: i32,
    pub frame_ms: f64,
    pub redundancy: usize,
    /// Largest datagram the link accepts (envelope included).
    pub max_datagram: usize,
}

impl Default for SenderParams {
    /// audiobridge defaults, with Linker's automatic start bitrate.
    fn default() -> Self {
        SenderParams {
            bitrate_bps: 128_000,
            complexity: 8,
            frame_ms: 5.0,
            redundancy: 2,
            max_datagram: proto::MAX_PACKET + proto::ENVELOPE_LEN,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SenderStats {
    pub packets: u64,
    pub bytes: u64,
    pub frames: u64,
    pub encode_errors: u64,
}

pub struct Sender {
    tag: u8,
    enc: Encoder,
    framer: Framer,
    packer: Packer,
    params: SenderParams,
    out: Vec<u8>,
    peak: u16,
    stats: SenderStats,
}

impl Sender {
    pub fn new(tag: u8, params: SenderParams) -> Result<Sender, CodecError> {
        let enc = Encoder::new(params.bitrate_bps, params.complexity, params.frame_ms)?;
        let mut packer = Packer::new(params.redundancy);
        packer.set_max_packet(params.max_datagram.saturating_sub(proto::ENVELOPE_LEN));
        Ok(Sender {
            tag,
            framer: Framer::new(enc.frame_size()),
            enc,
            packer,
            params,
            out: Vec::with_capacity(proto::MAX_PACKET + proto::ENVELOPE_LEN),
            peak: 0,
            stats: SenderStats::default(),
        })
    }

    pub fn params(&self) -> SenderParams {
        self.params
    }

    pub fn set_bitrate(&mut self, bps: i32) -> Result<(), CodecError> {
        self.enc.set_bitrate(bps)?;
        self.params.bitrate_bps = bps;
        Ok(())
    }

    /// The link's datagram limit changed (path MTU discovery).
    pub fn set_max_datagram(&mut self, n: usize) {
        self.params.max_datagram = n;
        self.packer
            .set_max_packet(n.saturating_sub(proto::ENVELOPE_LEN));
    }

    /// Collect captured PCM into frames; each complete frame is encoded and
    /// handed to `send` as a ready datagram. `capture_ts_us` is the wall
    /// clock (µs) when the *last* sample of `pcm` was captured; each frame is
    /// stamped with the capture time of its own last sample.
    pub fn push(&mut self, pcm: &[i16], capture_ts_us: u64, mut send: impl FnMut(&[u8])) {
        self.peak = self.peak.max(peak(pcm));
        let total = (self.framer.pending_frames() + pcm.len() / CHANNELS) as u64;
        let fs = self.framer.size() as u64;
        let mut done = 0u64;
        // Newest frame alone must fit one datagram: envelope + header + length.
        let max_frame = self
            .params
            .max_datagram
            .saturating_sub(proto::ENVELOPE_LEN + proto::HEADER_SIZE + 2);
        let Sender {
            tag,
            enc,
            framer,
            packer,
            out,
            stats,
            ..
        } = self;
        framer.push(pcm, |frame| {
            done += fs;
            // Samples captured after this frame's end, converted to µs.
            let after_us = (total - done) * 1_000_000 / RATE as u64;
            match enc.encode_max(frame, max_frame) {
                Ok(opus_frame) => {
                    let p = packer.pack(opus_frame);
                    proto::wrap(*tag, capture_ts_us.saturating_sub(after_us), p, out);
                    stats.packets += 1;
                    stats.frames += 1;
                    stats.bytes += out.len() as u64;
                    send(out);
                }
                Err(_) => stats.encode_errors += 1,
            }
        });
    }

    /// Largest |sample| captured since the previous call.
    pub fn take_peak(&mut self) -> u16 {
        std::mem::take(&mut self.peak)
    }

    pub fn stats(&self) -> SenderStats {
        self.stats
    }
}

/// Automatic Opus bitrate (kbit/s) driven by the receiver's 1 s reports:
/// step up by 32 after 10 clean reports, step down ×0.75 on loss/underrun.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitrateControl {
    pub kbps: u32,
    min: u32,
    max: u32,
    clean: u32,
    fixed: bool,
}

/// Starting point for automatic mode.
pub const AUTO_START_KBPS: u32 = 128;
const CLEAN_REPORTS_TO_RAISE: u32 = 10;
const RAISE_STEP_KBPS: u32 = 32;

impl BitrateControl {
    /// `requested = 0` → automatic within `min..=max`; otherwise fixed.
    pub fn new(requested: u32, min: u32, max: u32) -> BitrateControl {
        let (min, max) = (min.min(max), max.max(min));
        let fixed = requested != 0;
        BitrateControl {
            kbps: if fixed {
                requested
            } else {
                AUTO_START_KBPS.clamp(min, max)
            },
            min,
            max,
            clean: 0,
            fixed,
        }
    }

    pub fn is_auto(&self) -> bool {
        !self.fixed
    }

    /// Returns the new bitrate if it changed.
    pub fn on_feedback(
        &mut self,
        loss_permille: u16,
        plc_permille: u16,
        underruns: u16,
    ) -> Option<u32> {
        if self.fixed {
            return None;
        }
        let old = self.kbps;
        if loss_permille > 20 || plc_permille > 5 || underruns > 0 {
            self.kbps = ((self.kbps as f64 * 0.75) as u32).max(self.min);
            self.clean = 0;
        } else if loss_permille == 0 && plc_permille == 0 {
            self.clean += 1;
            if self.clean >= CLEAN_REPORTS_TO_RAISE {
                self.kbps = (self.kbps + RAISE_STEP_KBPS).min(self.max);
                self.clean = 0;
            }
        } else {
            self.clean = 0;
        }
        (self.kbps != old).then_some(self.kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_packets_and_timestamps() {
        let mut s = Sender::new(proto::TAG_SPEAKER, SenderParams::default()).unwrap();
        let mut got = Vec::new();
        // 441 frames per call (like a 44.1k-sized chunk): 240-frame Opus frames.
        for _ in 0..10 {
            s.push(&vec![1000i16; 441 * 2], 1_000_000, |d| got.push(d.to_vec()));
        }
        assert_eq!(got.len(), 4410 / 240);
        for (i, d) in got.iter().enumerate() {
            let (tag, ts, p) = proto::unwrap(d).unwrap();
            assert_eq!(tag, proto::TAG_SPEAKER);
            assert!(ts <= 1_000_000);
            let pk = proto::parse(p).unwrap();
            assert_eq!(pk.seq, i as u32);
            assert_eq!(pk.n, (i + 1).min(3));
        }
        assert_eq!(s.take_peak(), 1000);
        assert_eq!(s.take_peak(), 0);
        assert_eq!(s.stats().packets, got.len() as u64);
    }

    #[test]
    fn datagram_limit_is_respected() {
        let mut s = Sender::new(
            proto::TAG_MIC,
            SenderParams {
                bitrate_bps: 510_000,
                max_datagram: 300,
                frame_ms: 20.0,
                ..Default::default()
            },
        )
        .unwrap();
        let mut phase = 0.0f64;
        let mut max = 0;
        for _ in 0..50 {
            let pcm: Vec<i16> = (0..960 * 2)
                .map(|i| {
                    phase += 0.37 + (i % 7) as f64;
                    (phase.sin() * 20000.0) as i16
                })
                .collect();
            s.push(&pcm, 0, |d| max = max.max(d.len()));
        }
        // 510 kbit/s × 20 ms ≈ 1275-byte frames: the encoder is capped so
        // every datagram (envelope + header + newest frame) fits, and
        // redundant copies are added only when they also fit.
        assert_eq!(s.stats().packets, 50);
        assert!(max <= 300, "datagram {max} bytes > 300");
    }

    #[test]
    fn bitrate_auto_raises_slowly_and_drops_fast() {
        let mut b = BitrateControl::new(0, 64, 256);
        assert_eq!(b.kbps, 128);
        for _ in 0..9 {
            assert_eq!(b.on_feedback(0, 0, 0), None);
        }
        assert_eq!(b.on_feedback(0, 0, 0), Some(160));
        // Loss above 2 %: ×0.75.
        assert_eq!(b.on_feedback(50, 0, 0), Some(120));
        // An underrun also lowers it.
        assert_eq!(b.on_feedback(0, 0, 1), Some(90));
        // Small loss (recovered by redundancy) neither raises nor lowers.
        assert_eq!(b.on_feedback(5, 0, 0), None);
        for _ in 0..200 {
            b.on_feedback(0, 0, 0);
        }
        assert_eq!(b.kbps, 256, "capped at max");
        for _ in 0..50 {
            b.on_feedback(900, 900, 9);
        }
        assert_eq!(b.kbps, 64, "floored at min");
    }

    #[test]
    fn bitrate_fixed_never_changes() {
        let mut b = BitrateControl::new(192, 64, 256);
        assert!(!b.is_auto());
        assert_eq!(b.on_feedback(900, 900, 9), None);
        assert_eq!(b.kbps, 192);
    }
}
