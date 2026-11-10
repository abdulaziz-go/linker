//! Sequence reordering with redundancy recovery — port of audiobridge
//! `cmd/receiver/reorder.go`.
//!
//! Wi-Fi delivers packets out of order; [`Reorder`] puts frames back in `seq`
//! order. Every packet also carries copies of the previous frames
//! (see [`crate::proto`]), so a lost packet is usually recovered from the next
//! one; when it can't be recovered the frame is emitted as `None` and the
//! decoder does PLC.

use std::time::{Duration, Instant};

use crate::proto::Packet;

/// Ring size: how far ahead of the next frame to play a packet may be.
pub const SLOTS: usize = 64;
/// How long a hole is waited for before giving up on it (then PLC).
pub const MAX_WAIT: Duration = Duration::from_millis(15);
/// ...or give up as soon as this many frames are queued behind the hole.
pub const MAX_HELD: usize = 3;

/// Sender restart threshold: a packet this far *behind* the next frame is a
/// new sender session, not a late packet (Go: `d < -1000`).
const RESTART_BEHIND: i32 = -1000;

#[derive(Default)]
struct Slot {
    seq: u32,
    ok: bool,
    /// Recovered from a redundant copy (not the packet's own newest frame).
    red: bool,
    at: Option<Instant>,
    /// Owned and reused across frames (no per-frame allocation in steady state).
    data: Vec<u8>,
}

/// Reorders packets by `seq`, recovering lost frames from redundant copies.
pub struct Reorder {
    next: u32,
    started: bool,
    slots: [Slot; SLOTS],
    held: usize,

    /// Frames emitted from a redundant copy.
    pub recovered: u64,
    /// Frames that could not be recovered (emitted as `None`).
    pub lost: u64,
    /// Packets whose frames had all been played already.
    pub late: u64,
}

impl Default for Reorder {
    fn default() -> Self {
        Reorder {
            next: 0,
            started: false,
            slots: std::array::from_fn(|_| Slot::default()),
            held: 0,
            recovered: 0,
            lost: 0,
            late: 0,
        }
    }
}

impl Reorder {
    pub fn new() -> Reorder {
        Reorder::default()
    }

    /// Drop all held frames and expect `seq` next (Go `reset`).
    pub fn reset(&mut self, seq: u32) {
        for s in &mut self.slots {
            s.ok = false;
        }
        self.held = 0;
        self.next = seq;
        self.started = true;
    }

    /// Forget everything, including the expected `seq`: the next packet starts
    /// a fresh stream (used when the link changes). Counters are kept.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.ok = false;
        }
        self.held = 0;
        self.next = 0;
        self.started = false;
    }

    pub fn started(&self) -> bool {
        self.started
    }

    /// Frames currently held (waiting behind a hole or for `flush`).
    pub fn held(&self) -> usize {
        self.held
    }

    fn put(&mut self, seq: u32, frame: &[u8], red: bool, now: Instant) {
        if (seq.wrapping_sub(self.next) as i32) < 0 {
            return; // already played
        }
        let s = &mut self.slots[seq as usize % SLOTS];
        if s.ok && s.seq == seq {
            return;
        }
        if !s.ok {
            self.held += 1;
        }
        s.seq = seq;
        s.ok = true;
        s.red = red;
        s.at = Some(now);
        s.data.clear();
        s.data.extend_from_slice(frame);
    }

    /// Accept a packet. Returns `true` if the sender restarted (the decoder
    /// must be reset).
    pub fn add(&mut self, p: &Packet<'_>, now: Instant) -> bool {
        let mut restarted = false;
        if !self.started {
            // Go: p.Seq - uint32(p.N-1). `n` is >= 1 for a parsed packet;
            // saturating keeps a hand-built n == 0 from panicking.
            self.reset(p.seq.wrapping_sub(p.n.saturating_sub(1) as u32));
        }
        let d = p.seq.wrapping_sub(self.next) as i32;
        if d < RESTART_BEHIND || d >= SLOTS as i32 {
            self.reset(p.seq);
            restarted = true;
        } else if d < 0 {
            self.late += 1; // every frame of this packet was already played
            return false;
        }
        for i in 0..p.n {
            self.put(p.seq.wrapping_sub(i as u32), p.frames[i], i > 0, now);
        }
        restarted
    }

    /// Emit the frames that are ready, in order; `None` for a frame that
    /// could not be recovered (the decoder does PLC).
    pub fn flush(&mut self, now: Instant, mut emit: impl FnMut(Option<&[u8]>)) {
        while self.started {
            let next = self.next;
            let s = &mut self.slots[next as usize % SLOTS];
            if s.ok && s.seq == next {
                if s.red {
                    self.recovered += 1;
                }
                emit(Some(&s.data));
                s.ok = false;
                self.held -= 1;
                self.next = next.wrapping_add(1);
                continue;
            }
            if self.held == 0 {
                return;
            }
            // There is a hole: have we waited long enough, or have many
            // frames piled up behind it?
            let mut oldest = now;
            for s in &self.slots {
                if s.ok {
                    if let Some(at) = s.at {
                        if at < oldest {
                            oldest = at;
                        }
                    }
                }
            }
            if now.saturating_duration_since(oldest) < MAX_WAIT && self.held < MAX_HELD {
                return;
            }
            self.lost += 1;
            emit(None);
            self.next = next.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{parse, Packer, MAX_FRAMES};
    use rand::rngs::SmallRng;
    use rand::{Rng, SeedableRng};

    /// One test frame: its own number, LE.
    fn frame_no(f: Option<&[u8]>) -> i64 {
        match f {
            None => -1,
            Some(b) => u32::from_le_bytes(b.try_into().unwrap()) as i64,
        }
    }

    /// Hand-built packet: `frames[i]` is frame `seq - i`.
    fn pkt<'a>(seq: u32, frames: &[&'a [u8]]) -> Packet<'a> {
        let mut f: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        f[..frames.len()].copy_from_slice(frames);
        Packet {
            seq,
            n: frames.len(),
            frames: f,
        }
    }

    fn run_redundancy(seed: u64, loss: f64, reorder: f64) -> (Reorder, Vec<i64>) {
        let mut rng = SmallRng::seed_from_u64(seed);
        let mut pk = Packer::new(2);
        let mut ro = Reorder::new();
        let mut got = Vec::new(); // -1 = PLC
        let mut now = Instant::now();
        let mut pkts: Vec<Vec<u8>> = Vec::new();
        for i in 0..3000u32 {
            let b = pk.pack(&i.to_le_bytes());
            if rng.gen::<f64>() * 100.0 >= loss {
                pkts.push(b.to_vec());
            }
        }
        for k in 1..pkts.len() {
            if rng.gen::<f64>() * 100.0 < reorder {
                pkts.swap(k - 1, k);
            }
        }
        for b in &pkts {
            now += Duration::from_millis(10);
            let p = parse(b).expect("valid packet");
            ro.add(&p, now);
            ro.flush(now, |f| got.push(frame_no(f)));
        }
        ro.flush(now + Duration::from_secs(1), |f| got.push(frame_no(f)));
        (ro, got)
    }

    /// Port of `TestReorderRedundancy`: every "frame" carries its own number;
    /// the output must be complete and in order, with `None` (PLC) only for
    /// frames that could not be recovered.
    ///
    /// Go uses one PCG(1, 2) stream; our RNG differs, so every case is run
    /// for seeds 1..=5 and the Go bound is asserted for each seed.
    #[test]
    fn reorder_redundancy() {
        for (name, loss, reorder, max_lost) in [
            ("clean", 0.0, 0.0, 0),
            ("reorder only", 0.0, 20.0, 0),
            ("5% loss", 5.0, 0.0, 4),
            ("10% loss + reorder", 10.0, 10.0, 12),
        ] {
            for seed in 1..=5u64 {
                let (ro, got) = run_redundancy(seed, loss, reorder);
                for (i, &v) in got.iter().enumerate() {
                    assert!(
                        v == -1 || v == i as i64,
                        "{name} seed {seed}: frame {v} at position {i} — order broken"
                    );
                }
                eprintln!(
                    "{name} seed {seed}: frames {} | recovered {} PLC {} late {}",
                    got.len(),
                    ro.recovered,
                    ro.lost,
                    ro.late
                );
                assert!(
                    ro.lost <= max_lost,
                    "{name} seed {seed}: PLC {} (max {max_lost})",
                    ro.lost
                );
            }
        }
    }

    #[test]
    fn seq_wraps_across_u32_max() {
        let mut ro = Reorder::new();
        let t0 = Instant::now();
        let mut got = Vec::new();
        let start = u32::MAX - 4;
        let mut seqs = Vec::new();
        // Packer has no public seq setter: build packets by hand instead.
        let frames: Vec<[u8; 4]> = (0..10u32)
            .map(|i| start.wrapping_add(i).to_le_bytes())
            .collect();
        for i in 0..10usize {
            let seq = start.wrapping_add(i as u32);
            seqs.push(seq);
            // newest first, up to 2 redundant copies
            let mut fs: Vec<&[u8]> = vec![&frames[i]];
            if i >= 1 {
                fs.push(&frames[i - 1]);
            }
            if i >= 2 {
                fs.push(&frames[i - 2]);
            }
            // Lose the packet carrying the wrap point itself (seq 0).
            if seq == 0 {
                continue;
            }
            assert!(!ro.add(&pkt(seq, &fs), t0 + Duration::from_millis(i as u64)));
            ro.flush(t0 + Duration::from_millis(i as u64), |f| {
                got.push(f.map(|b| u32::from_le_bytes(b.try_into().unwrap())))
            });
        }
        let want: Vec<Option<u32>> = seqs.iter().copied().map(Some).collect();
        assert_eq!(got, want);
        assert_eq!((ro.lost, ro.late, ro.recovered), (0, 0, 1));
    }

    #[test]
    fn sender_restart_detected() {
        let t0 = Instant::now();
        let mut ro = Reorder::new();
        let mut got = Vec::new();
        let mut feed = |ro: &mut Reorder, seq: u32, t: u64| {
            let b = seq.to_le_bytes();
            let r = ro.add(&pkt(seq, &[&b]), t0 + Duration::from_millis(t));
            ro.flush(t0 + Duration::from_millis(t), |f| got.push(frame_no(f)));
            r
        };
        for s in 5000..5010 {
            assert!(!feed(&mut ro, s, 0));
        }
        // Jump back by more than 1000: a new sender session.
        assert!(feed(&mut ro, 0, 1));
        assert!(!feed(&mut ro, 1, 2));
        // next == 2 now. Exactly SLOTS ahead: also a restart.
        assert!(feed(&mut ro, 2 + SLOTS as u32, 3));
        assert!(!feed(&mut ro, 3 + SLOTS as u32, 4));
        // next == 4 + SLOTS. SLOTS - 1 ahead is a hole, not a restart.
        assert!(!feed(&mut ro, 4 + 2 * SLOTS as u32 - 1, 5));
        let mut want: Vec<i64> = (5000..5010).collect();
        want.extend([0, 1, 2 + SLOTS as i64, 3 + SLOTS as i64]);
        assert_eq!(got, want);
        assert_eq!(ro.held(), 1);
        assert_eq!(ro.late, 0);
    }

    #[test]
    fn late_packets_counted() {
        let t0 = Instant::now();
        let mut ro = Reorder::new();
        let mut got = Vec::new();
        let fr: Vec<[u8; 4]> = (0..10u32).map(u32::to_le_bytes).collect();
        for (s, f) in fr.iter().enumerate().take(6) {
            ro.add(&pkt(s as u32, &[f]), t0);
        }
        ro.flush(t0, |f| got.push(frame_no(f)));
        assert_eq!(got, [0, 1, 2, 3, 4, 5]);
        // Newest frame already played: late (a small jump back is not a restart).
        assert!(!ro.add(&pkt(3, &[&fr[3]]), t0));
        assert!(!ro.add(&pkt(5, &[&fr[5], &fr[4]]), t0));
        assert_eq!(ro.late, 2);
        assert_eq!(ro.held(), 0);
        // Packet whose newest frame is new: old redundant copies are ignored.
        assert!(!ro.add(&pkt(6, &[&fr[6], &fr[5], &fr[4]]), t0));
        assert_eq!(ro.held(), 1);
        ro.flush(t0, |f| got.push(frame_no(f)));
        assert_eq!(got, [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!((ro.late, ro.recovered, ro.lost), (2, 0, 0));
    }

    #[test]
    fn hole_emitted_after_max_wait() {
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);
        let mut ro = Reorder::new();
        let mut got = Vec::new();
        let fr: Vec<[u8; 4]> = (0..10u32).map(u32::to_le_bytes).collect();
        ro.add(&pkt(0, &[&fr[0]]), ms(0));
        ro.flush(ms(0), |f| got.push(frame_no(f)));
        // Frame 1 is missing; 2 arrives at 1 ms.
        ro.add(&pkt(2, &[&fr[2]]), ms(1));
        ro.flush(ms(1), |f| got.push(frame_no(f)));
        ro.flush(ms(15), |f| got.push(frame_no(f)));
        assert_eq!(got, [0], "waited only 14 ms");
        assert_eq!(ro.held(), 1);
        ro.flush(ms(16), |f| got.push(frame_no(f)));
        assert_eq!(got, [0, -1, 2]);
        assert_eq!(ro.lost, 1);
        // A late arrival of the given-up frame is late.
        ro.add(&pkt(1, &[&fr[1]]), ms(17));
        assert_eq!(ro.late, 1);
    }

    #[test]
    fn hole_emitted_after_max_held() {
        let t0 = Instant::now();
        let mut ro = Reorder::new();
        let mut got = Vec::new();
        let fr: Vec<[u8; 4]> = (0..10u32).map(u32::to_le_bytes).collect();
        ro.add(&pkt(0, &[&fr[0]]), t0);
        ro.flush(t0, |f| got.push(frame_no(f)));
        for (s, f) in fr.iter().enumerate().skip(2).take(MAX_HELD) {
            ro.flush(t0, |f| got.push(frame_no(f)));
            assert_eq!(got, [0], "held {} < MAX_HELD", ro.held());
            ro.add(&pkt(s as u32, &[f]), t0);
        }
        // MAX_HELD frames behind the hole: give up at once, no waiting.
        ro.flush(t0, |f| got.push(frame_no(f)));
        assert_eq!(got, [0, -1, 2, 3, 4]);
        assert_eq!((ro.lost, ro.held()), (1, 0));
    }

    #[test]
    fn redundant_copy_fills_hole() {
        let t0 = Instant::now();
        let mut ro = Reorder::new();
        let mut got = Vec::new();
        let fr: Vec<[u8; 4]> = (0..10u32).map(u32::to_le_bytes).collect();
        // First packet already carries history: start from its oldest frame.
        ro.add(&pkt(2, &[&fr[2], &fr[1]]), t0);
        ro.flush(t0, |f| got.push(frame_no(f)));
        // Packet 3 lost; packet 4 carries 3.
        ro.add(&pkt(4, &[&fr[4], &fr[3], &fr[2]]), t0);
        ro.flush(t0, |f| got.push(frame_no(f)));
        assert_eq!(got, [1, 2, 3, 4]);
        assert_eq!((ro.recovered, ro.lost), (2, 0));
    }

    #[test]
    fn clear_forgets_stream_keeps_counters() {
        let t0 = Instant::now();
        let mut ro = Reorder::new();
        let mut got = Vec::new();
        let fr: Vec<[u8; 4]> = (0..10u32).map(u32::to_le_bytes).collect();
        ro.add(&pkt(0, &[&fr[0]]), t0);
        ro.add(&pkt(2, &[&fr[2]]), t0);
        ro.add(&pkt(0, &[&fr[0]]), t0);
        ro.flush(t0, |f| got.push(frame_no(f)));
        ro.add(&pkt(0, &[&fr[0]]), t0); // late
        assert_eq!((ro.held(), ro.late), (1, 1));
        ro.clear();
        assert!(!ro.started());
        assert_eq!(ro.held(), 0);
        assert_eq!(ro.late, 1, "counters kept");
        // Nothing left to emit.
        ro.flush(t0 + Duration::from_secs(1), |f| got.push(frame_no(f)));
        assert_eq!(got, [0]);
        // A new stream at an arbitrary seq starts cleanly, no restart reported.
        assert!(!ro.add(&pkt(7, &[&fr[7]]), t0));
        assert!(ro.started());
        ro.flush(t0, |f| got.push(frame_no(f)));
        assert_eq!(got, [0, 7]);
    }
}
