//! Audio packet format — port of audiobridge `internal/proto/proto.go` (v3).
//!
//! Packet: 8-byte header + N Opus frames (newest first).
//!
//! ```text
//! [0:2]  magic 'A','B'
//! [2]    version (3)
//! [3]    N — frames in this packet (1 + redundancy)
//! [4:8]  seq — sequence number of the newest frame (u32 LE)
//! then N times: [u16 LE length][Opus frame]   — seq, seq-1, seq-2, …
//! ```
//!
//! Every packet also carries copies of the previous frames (an Opus frame is
//! ~100–250 bytes, so this is cheap): even if `redundancy` consecutive packets
//! are lost the audio is recovered. Frame: 48 kHz stereo Opus, 2.5/5/10 ms
//! (the duration is inside the Opus packet itself).
//!
//! Linker wraps each packet in a small [envelope](wrap) so audio can share
//! the QUIC link's datagrams with the camera.

pub const HEADER_SIZE: usize = 8;
pub const VERSION: u8 = 3;
/// Maximum packet size (no IP fragmentation on Wi-Fi). On a QUIC link the
/// [`Packer`] is further clamped to the datagram limit.
pub const MAX_PACKET: usize = 1400;
pub const MAX_FRAMES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("bad packet")]
pub struct BadPacket;

/// A parsed packet borrowing from the datagram. `frames[i]` is frame `seq - i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    pub seq: u32,
    pub n: usize,
    pub frames: [&'a [u8]; MAX_FRAMES],
}

pub fn parse(b: &[u8]) -> Result<Packet<'_>, BadPacket> {
    if b.len() < HEADER_SIZE || b[0] != b'A' || b[1] != b'B' || b[2] != VERSION {
        return Err(BadPacket);
    }
    let n = b[3] as usize;
    if !(1..=MAX_FRAMES).contains(&n) {
        return Err(BadPacket);
    }
    let seq = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let mut p = Packet {
        seq,
        n,
        frames: [&[]; MAX_FRAMES],
    };
    let mut b = &b[HEADER_SIZE..];
    for i in 0..n {
        if b.len() < 2 {
            return Err(BadPacket);
        }
        let l = u16::from_le_bytes([b[0], b[1]]) as usize;
        if b.len() < 2 + l || l == 0 {
            return Err(BadPacket);
        }
        p.frames[i] = &b[2..2 + l];
        b = &b[2 + l..];
    }
    if !b.is_empty() {
        return Err(BadPacket);
    }
    Ok(p)
}

/// Wraps Opus frames into packets, adding the last `redundancy` frames.
#[derive(Debug)]
pub struct Packer {
    redundancy: usize,
    max_packet: usize,
    seq: u32,
    /// `hist[0]` is the newest frame; `None` until that many frames were sent.
    hist: [Option<Vec<u8>>; MAX_FRAMES],
    buf: Vec<u8>,
}

impl Packer {
    /// `redundancy` is clamped to `0..=MAX_FRAMES-1`.
    pub fn new(redundancy: usize) -> Packer {
        Packer {
            redundancy: redundancy.min(MAX_FRAMES - 1),
            max_packet: MAX_PACKET,
            seq: 0,
            hist: Default::default(),
            buf: Vec::with_capacity(MAX_PACKET),
        }
    }

    pub fn redundancy(&self) -> usize {
        self.redundancy
    }

    pub fn set_redundancy(&mut self, r: usize) {
        self.redundancy = r.min(MAX_FRAMES - 1);
    }

    /// Lower the size limit (e.g. to a QUIC datagram's max payload). Never
    /// above [`MAX_PACKET`]. Redundant copies that don't fit are left out;
    /// the newest frame is always sent.
    pub fn set_max_packet(&mut self, n: usize) {
        self.max_packet = n.min(MAX_PACKET);
    }

    /// Sequence number the next packed frame will get.
    pub fn next_seq(&self) -> u32 {
        self.seq
    }

    /// Pack one frame. The returned slice is valid until the next call.
    pub fn pack(&mut self, frame: &[u8]) -> &[u8] {
        // Shift the history, reusing the oldest buffer (no allocation in
        // steady state).
        let mut last = self.hist[MAX_FRAMES - 1].take().unwrap_or_default();
        self.hist.rotate_right(1);
        last.clear();
        last.extend_from_slice(frame);
        self.hist[0] = Some(last);

        let mut n = 1;
        let mut size = HEADER_SIZE + 2 + frame.len();
        while n <= self.redundancy {
            let Some(h) = &self.hist[n] else { break };
            if size + 2 + h.len() > self.max_packet {
                break;
            }
            size += 2 + h.len();
            n += 1;
        }

        let b = &mut self.buf;
        b.clear();
        b.extend_from_slice(&[b'A', b'B', VERSION, n as u8]);
        b.extend_from_slice(&self.seq.to_le_bytes());
        for h in self.hist.iter().take(n) {
            let h = h.as_deref().expect("present for i < n");
            b.extend_from_slice(&(h.len() as u16).to_le_bytes());
            b.extend_from_slice(h);
        }
        self.seq = self.seq.wrapping_add(1);
        &self.buf
    }
}

// ---------------------------------------------------------------------------
// Linker envelope
// ---------------------------------------------------------------------------

/// Datagram tag of the microphone stream (Mac → Ubuntu).
pub const TAG_MIC: u8 = 0xA1;
/// Datagram tag of the speaker stream (Ubuntu → Mac).
pub const TAG_SPEAKER: u8 = 0xA2;
/// `tag u8` + `capture_ts_us u64 LE`.
pub const ENVELOPE_LEN: usize = 9;

/// Is this datagram (first byte) an audio datagram?
pub fn is_audio_tag(b: u8) -> bool {
    b == TAG_MIC || b == TAG_SPEAKER
}

/// Build `tag || capture_ts_us || packet` into `out` (cleared first).
pub fn wrap(tag: u8, capture_ts_us: u64, packet: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(ENVELOPE_LEN + packet.len());
    out.push(tag);
    out.extend_from_slice(&capture_ts_us.to_le_bytes());
    out.extend_from_slice(packet);
}

/// Split an envelope into `(tag, capture_ts_us, packet)`.
pub fn unwrap(d: &[u8]) -> Option<(u8, u64, &[u8])> {
    if d.len() < ENVELOPE_LEN || !is_audio_tag(d[0]) {
        return None;
    }
    let ts = u64::from_le_bytes(d[1..9].try_into().ok()?);
    Some((d[0], ts, &d[ENVELOPE_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of `TestParseRejectsOldVersion`.
    #[test]
    fn parse_rejects_old_version() {
        let mut b = vec![0u8; 16 + 960];
        b[0] = b'A';
        b[1] = b'B';
        b[2] = 2;
        b[3] = 2;
        assert!(parse(&b).is_err(), "old v2 packet must be rejected");
    }

    /// Port of `TestPackerFitsMTU`.
    #[test]
    fn packer_fits_mtu() {
        let mut pk = Packer::new(3);
        let big = vec![0u8; 1000];
        for _ in 0..5 {
            let n = pk.pack(&big).len();
            assert!(n <= MAX_PACKET, "packet {n} bytes > MTU");
        }
    }

    #[test]
    fn packer_respects_lower_datagram_limit() {
        let mut pk = Packer::new(3);
        pk.set_max_packet(1100);
        for i in 0..5 {
            let p = pk.pack(&[7u8; 500]).to_vec();
            assert!(p.len() <= 1100);
            // first packet has no history yet; afterwards only one copy fits
            assert_eq!(parse(&p).unwrap().n, if i == 0 { 1 } else { 2 });
        }
    }

    #[test]
    fn roundtrip_with_redundancy_newest_first() {
        let mut pk = Packer::new(2);
        let p0 = pk.pack(b"zero").to_vec();
        let p1 = pk.pack(b"one").to_vec();
        let p2 = pk.pack(b"two").to_vec();
        let p3 = pk.pack(b"three").to_vec();
        let a = parse(&p0).unwrap();
        assert_eq!((a.seq, a.n), (0, 1));
        assert_eq!(a.frames[0], b"zero");
        let b = parse(&p1).unwrap();
        assert_eq!((b.seq, b.n), (1, 2));
        assert_eq!(&b.frames[..2], &[&b"one"[..], &b"zero"[..]]);
        let c = parse(&p2).unwrap();
        assert_eq!((c.seq, c.n), (2, 3));
        let d = parse(&p3).unwrap();
        assert_eq!((d.seq, d.n), (3, 3));
        assert_eq!(&d.frames[..3], &[&b"three"[..], &b"two"[..], &b"one"[..]]);
        // Header layout is byte-exact with audiobridge.
        assert_eq!(&p3[..8], &[b'A', b'B', 3, 3, 3, 0, 0, 0]);
        assert_eq!(&p3[8..10], &5u16.to_le_bytes());
    }

    #[test]
    fn redundancy_zero_and_clamped() {
        let mut pk = Packer::new(0);
        pk.pack(b"a");
        assert_eq!(parse(pk.pack(b"b")).unwrap().n, 1);
        let mut pk = Packer::new(99);
        assert_eq!(pk.redundancy(), MAX_FRAMES - 1);
        for f in [b"1", b"2", b"3", b"4", b"5"] {
            pk.pack(f);
        }
        assert_eq!(parse(pk.pack(b"6")).unwrap().n, MAX_FRAMES);
    }

    #[test]
    fn parse_rejects_malformed() {
        let mut pk = Packer::new(2);
        pk.pack(b"x");
        let good = pk.pack(b"yy").to_vec();
        assert!(parse(&good).is_ok());
        assert!(parse(&good[..good.len() - 1]).is_err(), "truncated");
        let mut extra = good.clone();
        extra.push(0);
        assert!(parse(&extra).is_err(), "trailing bytes");
        let mut zero_n = good.clone();
        zero_n[3] = 0;
        assert!(parse(&zero_n).is_err());
        let mut many = good.clone();
        many[3] = 5;
        assert!(parse(&many).is_err());
        let mut magic = good.clone();
        magic[0] = b'X';
        assert!(parse(&magic).is_err());
        // zero-length frame
        let z = [b'A', b'B', 3, 1, 0, 0, 0, 0, 0, 0];
        assert!(parse(&z).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn seq_wraps() {
        let mut pk = Packer::new(1);
        pk.seq = u32::MAX;
        let a = parse(pk.pack(b"a")).unwrap().seq;
        let b = parse(pk.pack(b"b")).unwrap().seq;
        assert_eq!((a, b), (u32::MAX, 0));
    }

    #[test]
    fn envelope_roundtrip_and_demux() {
        let mut out = Vec::new();
        wrap(TAG_SPEAKER, 0x0102_0304_0506_0708, b"pkt", &mut out);
        assert_eq!(out.len(), ENVELOPE_LEN + 3);
        let (tag, ts, p) = unwrap(&out).unwrap();
        assert_eq!(
            (tag, ts, p),
            (TAG_SPEAKER, 0x0102_0304_0506_0708, &b"pkt"[..])
        );
        assert!(unwrap(&out[..8]).is_none());
        // Camera fragments (0xC1) are never mistaken for audio.
        assert!(!is_audio_tag(0xC1));
        out[0] = 0xC1;
        assert!(unwrap(&out).is_none());
    }
}
