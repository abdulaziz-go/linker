//! Camera media datagrams.
//!
//! Each encoded H.264 access unit (Annex-B) is split into fragments that fit
//! in one QUIC datagram. Datagrams are unreliable: a frame missing any
//! fragment is dropped (never retransmitted), and the sink asks the source
//! for a keyframe to resynchronise.
//!
//! ```text
//! 0      1      2          6          8           10              18
//! +------+------+----------+----------+-----------+---------------+---------
//! |magic |flags | frame_id | frag_idx | frag_count| capture_ts_us | payload…
//! | 0xC1 |bit0=K| u32 BE   | u16 BE   | u16 BE    | u64 BE        |
//! +------+------+----------+----------+-----------+---------------+---------
//! ```

use std::collections::BTreeMap;

pub const MAGIC: u8 = 0xC1;
pub const HEADER_LEN: usize = 18;
const FLAG_KEYFRAME: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragHeader {
    pub keyframe: bool,
    pub frame_id: u32,
    pub frag_idx: u16,
    pub frag_count: u16,
    pub capture_ts_us: u64,
}

impl FragHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(MAGIC);
        out.push(if self.keyframe { FLAG_KEYFRAME } else { 0 });
        out.extend_from_slice(&self.frame_id.to_be_bytes());
        out.extend_from_slice(&self.frag_idx.to_be_bytes());
        out.extend_from_slice(&self.frag_count.to_be_bytes());
        out.extend_from_slice(&self.capture_ts_us.to_be_bytes());
    }

    pub fn parse(buf: &[u8]) -> Option<(FragHeader, &[u8])> {
        if buf.len() < HEADER_LEN || buf[0] != MAGIC {
            return None;
        }
        let h = FragHeader {
            keyframe: buf[1] & FLAG_KEYFRAME != 0,
            frame_id: u32::from_be_bytes(buf[2..6].try_into().ok()?),
            frag_idx: u16::from_be_bytes(buf[6..8].try_into().ok()?),
            frag_count: u16::from_be_bytes(buf[8..10].try_into().ok()?),
            capture_ts_us: u64::from_be_bytes(buf[10..18].try_into().ok()?),
        };
        if h.frag_count == 0 || h.frag_idx >= h.frag_count {
            return None;
        }
        Some((h, &buf[HEADER_LEN..]))
    }
}

/// Split one encoded frame into datagram payloads no larger than `max_datagram`.
pub fn fragment(
    frame: &[u8],
    frame_id: u32,
    keyframe: bool,
    capture_ts_us: u64,
    max_datagram: usize,
) -> Vec<Vec<u8>> {
    let chunk = max_datagram.saturating_sub(HEADER_LEN).max(1);
    let count = frame.len().div_ceil(chunk).max(1);
    assert!(count <= u16::MAX as usize, "frame too large to fragment");
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * chunk;
        let end = (start + chunk).min(frame.len());
        let mut d = Vec::with_capacity(HEADER_LEN + end - start);
        FragHeader {
            keyframe,
            frame_id,
            frag_idx: i as u16,
            frag_count: count as u16,
            capture_ts_us,
        }
        .write(&mut d);
        d.extend_from_slice(&frame[start..end]);
        out.push(d);
    }
    out
}

/// A fully reassembled frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub frame_id: u32,
    pub keyframe: bool,
    pub capture_ts_us: u64,
    pub data: Vec<u8>,
}

struct Partial {
    keyframe: bool,
    capture_ts_us: u64,
    frags: Vec<Option<Vec<u8>>>,
    received: usize,
}

/// Reassembles fragments into frames. Frames are delivered strictly in
/// increasing `frame_id` order; anything older than the last delivered frame
/// is discarded, and incomplete frames that are overtaken count as lost.
#[derive(Default)]
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    last_delivered: Option<u32>,
    /// Frames known lost since the last call to [`Reassembler::take_lost`].
    lost: u32,
}

/// How many incomplete frames we keep in flight before giving up on the oldest.
const MAX_PENDING: usize = 8;

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one datagram; returns a frame when one completes.
    pub fn push(&mut self, datagram: &[u8]) -> Option<Frame> {
        let (h, payload) = FragHeader::parse(datagram)?;
        if let Some(last) = self.last_delivered {
            // wrapping-aware "is h.frame_id <= last"
            if (h.frame_id.wrapping_sub(last) as i32) <= 0 {
                return None;
            }
        }
        let p = self.partial.entry(h.frame_id).or_insert_with(|| Partial {
            keyframe: h.keyframe,
            capture_ts_us: h.capture_ts_us,
            frags: vec![None; h.frag_count as usize],
            received: 0,
        });
        if p.frags.len() != h.frag_count as usize {
            return None; // inconsistent header; ignore
        }
        let slot = &mut p.frags[h.frag_idx as usize];
        if slot.is_none() {
            *slot = Some(payload.to_vec());
            p.received += 1;
        }
        if p.received == p.frags.len() {
            let p = self.partial.remove(&h.frame_id).expect("present");
            // Everything older than this frame that is still incomplete is lost.
            let older: Vec<u32> = self
                .partial
                .keys()
                .copied()
                .filter(|id| (id.wrapping_sub(h.frame_id) as i32) < 0)
                .collect();
            for id in older {
                self.partial.remove(&id);
                self.lost += 1;
            }
            if let Some(last) = self.last_delivered {
                let gap = h.frame_id.wrapping_sub(last).saturating_sub(1);
                // frames we never saw a single fragment of are also lost
                self.lost = self.lost.max(gap);
            }
            self.last_delivered = Some(h.frame_id);
            let mut data = Vec::with_capacity(
                p.frags
                    .iter()
                    .map(|f| f.as_ref().map_or(0, |v| v.len()))
                    .sum(),
            );
            for f in p.frags.into_iter().flatten() {
                data.extend_from_slice(&f);
            }
            return Some(Frame {
                frame_id: h.frame_id,
                keyframe: p.keyframe,
                capture_ts_us: p.capture_ts_us,
                data,
            });
        }
        while self.partial.len() > MAX_PENDING {
            let oldest = *self.partial.keys().next().expect("non-empty");
            self.partial.remove(&oldest);
            self.lost += 1;
        }
        None
    }

    /// Number of frames lost since the last call.
    pub fn take_lost(&mut self) -> u32 {
        std::mem::take(&mut self.lost)
    }

    /// Forget all state (e.g. after a stream restart).
    pub fn reset(&mut self) {
        self.partial.clear();
        self.last_delivered = None;
        self.lost = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = FragHeader {
            keyframe: true,
            frame_id: 77,
            frag_idx: 2,
            frag_count: 5,
            capture_ts_us: 123456789,
        };
        let mut buf = Vec::new();
        h.write(&mut buf);
        buf.extend_from_slice(b"xyz");
        let (p, payload) = FragHeader::parse(&buf).unwrap();
        assert_eq!(p, h);
        assert_eq!(payload, b"xyz");
        assert!(FragHeader::parse(&buf[..10]).is_none());
    }

    #[test]
    fn fragment_and_reassemble_out_of_order() {
        let frame: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let mut frags = fragment(&frame, 1, true, 42, 1200);
        assert!(frags.iter().all(|f| f.len() <= 1200));
        frags.reverse();
        let mut r = Reassembler::new();
        let mut out = None;
        for f in &frags {
            if let Some(fr) = r.push(f) {
                out = Some(fr);
            }
        }
        let out = out.unwrap();
        assert_eq!(out.data, frame);
        assert!(out.keyframe);
        assert_eq!(out.capture_ts_us, 42);
        assert_eq!(r.take_lost(), 0);
    }

    #[test]
    fn incomplete_frame_counts_as_lost_and_late_frames_dropped() {
        let mut r = Reassembler::new();
        let a = fragment(&[1u8; 3000], 1, true, 0, 1200);
        let b = fragment(&[2u8; 100], 2, false, 0, 1200);
        // Frame 1 loses its last fragment.
        r.push(&a[0]);
        r.push(&a[1]);
        assert!(r.push(&b[0]).is_some());
        assert_eq!(r.take_lost(), 1);
        // Late fragment of frame 1 is ignored.
        assert!(r.push(&a[2]).is_none());
    }

    #[test]
    fn gaps_count_as_loss() {
        let mut r = Reassembler::new();
        r.push(&fragment(&[0u8; 10], 1, true, 0, 1200)[0]).unwrap();
        r.push(&fragment(&[0u8; 10], 5, false, 0, 1200)[0]).unwrap();
        assert_eq!(r.take_lost(), 3);
    }

    #[test]
    fn duplicate_fragments_ignored() {
        let mut r = Reassembler::new();
        let f = fragment(&[9u8; 2000], 3, false, 0, 1200);
        assert!(r.push(&f[0]).is_none());
        assert!(r.push(&f[0]).is_none());
        assert_eq!(r.push(&f[1]).unwrap().data.len(), 2000);
    }
}
