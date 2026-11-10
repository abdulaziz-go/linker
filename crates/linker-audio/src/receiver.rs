//! Receive path — port of the packet loop in audiobridge
//! `cmd/receiver/main.go`: parse → reorder → Opus decode (or PLC for a frame
//! that never arrived) → adaptive jitter buffer.
//!
//! The owner calls [`Receiver::on_packet`] for every packet and
//! [`Receiver::on_tick`] about every 5 ms (the Go loop's 5 ms read deadline)
//! so frames waiting on a hole are released on time even when no packet
//! arrives. The jitter buffer is shared with the audio callback that pulls
//! from it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::codec::{CodecError, Decoder, MAX_FRAME_SIZE};
use crate::jitter::JitterBuffer;
use crate::proto;
use crate::reorder::Reorder;
use crate::CHANNELS;

/// How often the owner should call [`Receiver::on_tick`].
pub const TICK: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiverStats {
    /// Valid packets received.
    pub packets: u64,
    /// Packets that failed to parse.
    pub bad: u64,
    /// Frames recovered from a redundant copy in a later packet.
    pub recovered: u64,
    /// Frames that never arrived → Opus PLC.
    pub plc: u64,
    /// Packets whose frames had all been played already.
    pub late: u64,
    /// Frames decoded (including PLC).
    pub frames: u64,
    /// Sender restarts / stream resets detected.
    pub restarts: u64,
    pub decode_errors: u64,
    /// Highest packet sequence number seen (for loss accounting).
    pub max_seq: u32,
}

pub struct Receiver {
    reorder: Reorder,
    dec: Decoder,
    jb: Arc<Mutex<JitterBuffer>>,
    pcm: Vec<i16>,
    disc: bool,
    stats: ReceiverStats,
    seen_any: bool,
}

impl Receiver {
    pub fn new(jb: Arc<Mutex<JitterBuffer>>) -> Result<Receiver, CodecError> {
        Ok(Receiver {
            reorder: Reorder::new(),
            dec: Decoder::new()?,
            jb,
            pcm: vec![0; MAX_FRAME_SIZE * CHANNELS],
            disc: false,
            stats: ReceiverStats::default(),
            seen_any: false,
        })
    }

    pub fn jitter(&self) -> &Arc<Mutex<JitterBuffer>> {
        &self.jb
    }

    /// Feed one audiobridge v3 packet (envelope already removed). Returns
    /// false if it was malformed.
    pub fn on_packet(&mut self, packet: &[u8], now: Instant) -> Result<(), proto::BadPacket> {
        let pk = match proto::parse(packet) {
            Ok(p) => p,
            Err(e) => {
                self.stats.bad += 1;
                return Err(e);
            }
        };
        self.stats.packets += 1;
        if !self.seen_any || (pk.seq.wrapping_sub(self.stats.max_seq) as i32) > 0 {
            self.stats.max_seq = pk.seq;
            self.seen_any = true;
        }
        if self.reorder.add(&pk, now) {
            // The sender restarted — start the decoder fresh (Go: dec.Close + NewDecoder).
            self.restart_decoder();
        }
        self.flush(now);
        Ok(())
    }

    /// Release frames whose hole timed out (call every [`TICK`]).
    pub fn on_tick(&mut self, now: Instant) {
        self.flush(now);
    }

    /// The stream was interrupted (link lost / new session): forget the
    /// reorder state and decoder; the next audio fades in.
    pub fn reset(&mut self) {
        self.reorder.clear();
        self.restart_decoder();
        self.seen_any = false;
    }

    fn restart_decoder(&mut self) {
        match Decoder::new() {
            Ok(d) => self.dec = d,
            Err(e) => {
                // Can't happen with valid constants; keep the old decoder.
                debug_assert!(false, "decoder: {e}");
            }
        }
        self.stats.restarts += 1;
        self.disc = true;
    }

    fn flush(&mut self, now: Instant) {
        let Receiver {
            reorder,
            dec,
            jb,
            pcm,
            disc,
            stats,
            ..
        } = self;
        reorder.flush(now, |frame| {
            // frame = None: lost — PLC fills it.
            let n = match dec.decode(frame, pcm) {
                Ok(n) => n,
                Err(_) => {
                    stats.decode_errors += 1;
                    dec.decode(None, pcm).unwrap_or(0) // corrupt frame — PLC
                }
            };
            stats.frames += 1;
            if n > 0 {
                jb.lock().expect("jitter lock").push(&pcm[..n], now, *disc);
                *disc = false;
            }
        });
        stats.recovered = reorder.recovered;
        stats.plc = reorder.lost;
        stats.late = reorder.late;
    }

    pub fn stats(&self) -> ReceiverStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sender::{Sender, SenderParams};

    fn tone(n: usize, phase: &mut f64) -> Vec<i16> {
        let mut v = Vec::with_capacity(n * 2);
        for _ in 0..n {
            let s = (8000.0 * phase.sin()) as i16;
            *phase += 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
            v.push(s);
            v.push(s);
        }
        v
    }

    #[test]
    fn end_to_end_clean_stream_plays_continuously() {
        let jb = Arc::new(Mutex::new(JitterBuffer::new(48_000, 2, 6, 200)));
        let mut rx = Receiver::new(jb.clone()).unwrap();
        let mut tx = Sender::new(proto::TAG_MIC, SenderParams::default()).unwrap();
        let t0 = Instant::now();
        let mut phase = 0.0;
        let mut out = vec![0i16; 128 * 2];
        let mut pulled = 0usize;
        // 2 s: 5 ms of capture every 5 ms, 128-frame pulls in between.
        let mut next_pull = 0.0f64;
        for step in 0..400u32 {
            let now = t0 + Duration::from_millis(step as u64 * 5);
            let pcm = tone(240, &mut phase);
            let mut pkts = Vec::new();
            tx.push(&pcm, 0, |d| pkts.push(d.to_vec()));
            for d in pkts {
                let (_, _, p) = proto::unwrap(&d).unwrap();
                rx.on_packet(p, now).unwrap();
            }
            rx.on_tick(now);
            while next_pull <= step as f64 * 5.0 {
                jb.lock().unwrap().pull(&mut out, now);
                pulled += 1;
                next_pull += 128.0 / 48.0;
            }
        }
        let st = rx.stats();
        assert_eq!(st.packets, 400);
        assert_eq!(st.plc, 0);
        assert_eq!(st.bad, 0);
        let jst = jb.lock().unwrap().stats();
        assert!(jst.playing, "{jst:?}");
        assert_eq!(jst.underruns, 0, "{jst:?}");
        assert!(pulled > 700);
    }

    #[test]
    fn garbage_is_counted_not_fatal() {
        let jb = Arc::new(Mutex::new(JitterBuffer::new(48_000, 2, 6, 200)));
        let mut rx = Receiver::new(jb).unwrap();
        assert!(rx.on_packet(b"nonsense", Instant::now()).is_err());
        assert_eq!(rx.stats().bad, 1);
    }
}
