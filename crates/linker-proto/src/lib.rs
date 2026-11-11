//! Linker wire protocol.
//!
//! This crate contains only data types and pure encode/decode logic — no I/O.
//!
//! # Wire format
//!
//! Control-plane messages travel on reliable QUIC streams as length-prefixed
//! frames:
//!
//! ```text
//! +----------------+--------------+------------------------------+
//! | len: u32 (BE)  | kind: u16 BE | body: postcard(payload)      |
//! +----------------+--------------+------------------------------+
//!   len = 2 + body.len()
//! ```
//!
//! The `kind` tag is explicit and stable. A receiver that sees a `kind` it does
//! not know decodes it to [`Message::Unknown`] and ignores it, so a newer peer
//! never breaks an older one. Payload structs are only ever *extended* by
//! introducing a new `kind`; existing kinds are frozen once released.
//!
//! Camera media uses QUIC datagrams with the fixed binary header in [`media`];
//! audio datagrams start with a different first byte (see `linker-audio`'s
//! `proto` module), so both share the connection's datagram channel.

pub mod keycode;
pub mod media;

use serde::{Deserialize, Serialize};

/// Protocol version exchanged in [`Hello`]. Bump when semantics change.
pub const PROTO_VERSION: u32 = 1;
/// Oldest peer protocol version we can still talk to.
pub const MIN_PROTO_VERSION: u32 = 1;
/// ALPN identifier used on the QUIC/TLS handshake.
pub const ALPN: &[u8] = b"linker/1";
/// Upper bound for a single control frame (guards against garbage lengths).
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("frame too short")]
    Short,
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
    #[error("payload codec error: {0}")]
    Codec(#[from] postcard::Error),
}

// ---------------------------------------------------------------------------
// Payload types
// ---------------------------------------------------------------------------

/// Features a device advertises in its [`Hello`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Feature {
    Clipboard,
    Notifications,
    Dnd,
    KvmServer,
    KvmClient,
    CameraSource,
    CameraSink,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub proto_version: u32,
    pub device_id: String,
    pub device_name: String,
    pub device_type: String,
    pub features: Vec<Feature>,
}

/// Heartbeat. `t_us` is the sender's wall clock (µs since UNIX epoch); the
/// pong echoes it and adds the responder's clock so both sides can estimate
/// RTT and clock offset (used for camera glass-to-glass latency).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub seq: u64,
    pub t_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub seq: u64,
    pub echo_t_us: u64,
    pub t_us: u64,
}

/// Pairing confirmation, sent after the user compared the SAS code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairConfirm {
    pub accepted: bool,
}

/// 32-byte content fingerprint used for clipboard echo suppression.
pub type ContentHash = [u8; 32];

/// Small text clipboard payload, sent inline on the control stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipText {
    pub hash: ContentHash,
    pub text: String,
    pub html: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipKind {
    /// Large text (above the inline threshold).
    Text,
    /// PNG image.
    Image,
    /// One or more files.
    Files,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipItemMeta {
    pub name: String,
    pub size: u64,
}

/// Advertisement of a heavy clipboard payload; the receiver pulls it on a
/// dedicated bidirectional stream so it never blocks the control stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipOffer {
    pub id: u64,
    pub hash: ContentHash,
    pub kind: ClipKind,
    pub total_size: u64,
    pub items: Vec<ClipItemMeta>,
}

/// First frame on a bulk stream: "send me offer `id`".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipPull {
    pub id: u64,
}

/// Response header on a bulk stream. It is followed on the same stream by
/// the raw bytes of each item, back to back (`wire_size` bytes each), so
/// payloads of any size stream without being buffered in a single frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipBlob {
    pub id: u64,
    /// `false` if the offer expired (clipboard changed again meanwhile).
    pub available: bool,
    /// For Text: one item (UTF-8). Image: one item (PNG). Files: one per file.
    pub items: Vec<BlobItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobItem {
    pub name: String,
    /// Bytes that follow on the stream for this item.
    pub wire_size: u64,
    /// Size after decompression.
    pub raw_size: u64,
    /// Item bytes are zstd-compressed.
    pub zstd: bool,
}

/// A mirrored notification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub app: String,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub timestamp_ms: u64,
}

/// Do-Not-Disturb state. `changed_at_ms` is when the sender's state last
/// changed locally; on reconnect the most recent change wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dnd {
    pub enabled: bool,
    pub changed_at_ms: u64,
}

/// Screen edge, from the perspective of the machine that owns the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

/// Modifier bitmask used to re-sync state on [`KvmEnter`].
pub mod modifiers {
    pub const SHIFT: u8 = 1 << 0;
    pub const CTRL: u8 = 1 << 1;
    pub const ALT: u8 = 1 << 2;
    pub const META: u8 = 1 << 3;
}

/// Server → client: the pointer crossed into the client's screen through
/// the client's `edge` at `ratio` (0.0–1.0 along that edge).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct KvmEnter {
    pub edge: Edge,
    pub ratio: f32,
    pub modifiers: u8,
}

/// Server → client: control returns to the server; release all held keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvmLeave;

/// Client → server: the client's pointer hit the return edge; hand back
/// control at `ratio` along that edge.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct KvmReturn {
    pub ratio: f32,
}

/// Client → server: dynamic injection readiness. `ready` is true only when the
/// client can really inject right now (macOS: Accessibility trusted and event
/// posting works). Sent on link-up, re-advertised periodically, and on every
/// change. The server never hands control to a client that is not ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvmReady {
    pub ready: bool,
}

/// Client → server: acknowledges a [`KvmEnter`] and confirms it is now
/// injecting. The server requires this before it grabs the local devices; no
/// ack within the handoff timeout aborts the handoff and keeps input local.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvmAck;

/// Client → server: periodic "injecting OK" heartbeat while control is on the
/// client. If these stop arriving the server releases the grab (the client may
/// have died or wedged without dropping the link).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvmHeartbeat;

/// Client → server: injection failed (e.g. Accessibility revoked mid-session).
/// The server releases the grab at once so the user is never locked out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvmError {
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseMove {
    pub dx: i32,
    pub dy: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseButtonEvent {
    pub button: MouseButton,
    pub pressed: bool,
}

/// Scroll in "lines" (positive dy = scroll up / away from user, positive dx = right).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scroll {
    pub dx: i32,
    pub dy: i32,
}

/// Key event using the hardware-independent USB HID usage code (page 0x07).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub hid: u16,
    pub pressed: bool,
}

/// Sink → source: start the camera stream with these parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CamStart {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CamStop;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CamState {
    Streaming,
    Stopped,
    /// Camera in use by another app or temporarily unavailable.
    Busy,
    /// Permission denied or no camera.
    Unavailable,
}

/// Source → sink: camera state changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CamStatus {
    pub state: CamState,
    pub detail: String,
}

/// Sink → source: periodic quality report (about once per second).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CamFeedback {
    /// Fraction of frames lost in the last report window, in 1/1000.
    pub loss_permille: u16,
    /// Median capture→display latency in ms (clock-offset corrected).
    pub latency_ms: u32,
    pub fps_received: u16,
    pub request_keyframe: bool,
}

/// Which audio stream a message is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioStream {
    /// Mac microphone → Ubuntu virtual source "Linker Microphone".
    Mic,
    /// Ubuntu virtual sink "Linker Speaker" → Mac default output.
    Speaker,
}

impl AudioStream {
    pub fn name(self) -> &'static str {
        match self {
            AudioStream::Mic => "mic",
            AudioStream::Speaker => "speaker",
        }
    }
}

/// Receiver → sender: start sending `stream` with these encoder settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStart {
    pub stream: AudioStream,
    /// Opus bitrate; 0 = automatic (sender adapts from feedback within
    /// `min_bitrate_kbps..=max_bitrate_kbps`).
    pub bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    /// Opus frame duration in 1/10 ms (25, 50, 100 or 200).
    pub frame_ms_x10: u16,
    /// Previous frames repeated in every packet (0..=3).
    pub redundancy: u8,
    /// Opus complexity 0..=10.
    pub complexity: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStop {
    pub stream: AudioStream,
}

/// Sender → receiver: state of an audio stream's capture side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStatus {
    pub stream: AudioStream,
    pub state: CamState,
    pub detail: String,
    /// Native rate of the capture device (the stream itself is 48 kHz).
    pub sample_rate: u32,
    /// Current Opus bitrate (re-sent whenever automatic mode changes it).
    pub bitrate_kbps: u32,
}

/// Receiver → sender: quality report, about once per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFeedback {
    pub stream: AudioStream,
    /// Frames that never arrived in any packet (Opus PLC filled them), 1/1000.
    pub plc_permille: u16,
    /// Datagrams missing in the report window, 1/1000 (before redundancy).
    pub loss_permille: u16,
    pub buffer_ms: u16,
    pub target_ms: u16,
    pub peak_gap_ms: u16,
    /// Underruns since the previous report.
    pub underruns: u16,
    /// Median capture → playout latency, ms (clock-offset corrected).
    pub latency_ms: u16,
}

// ---------------------------------------------------------------------------
// Message enum with stable kind tags
// ---------------------------------------------------------------------------

macro_rules! messages {
    ($($kind:literal => $variant:ident($ty:ty)),* $(,)?) => {
        /// Every control-plane message. `Unknown` is produced when decoding a
        /// kind this build does not understand.
        #[derive(Clone, Debug, PartialEq)]
        pub enum Message {
            $($variant($ty),)*
            Unknown { kind: u16 },
        }

        impl Message {
            /// Stable wire tag.
            pub fn kind(&self) -> u16 {
                match self {
                    $(Message::$variant(_) => $kind,)*
                    Message::Unknown { kind } => *kind,
                }
            }

            /// Encode as `kind || postcard(body)` (no length prefix).
            pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
                let mut out = Vec::with_capacity(32);
                out.extend_from_slice(&self.kind().to_be_bytes());
                match self {
                    $(Message::$variant(v) => {
                        out = postcard::to_extend(v, out)?;
                    })*
                    Message::Unknown { .. } => {}
                }
                Ok(out)
            }

            /// Decode `kind || body`. Unknown kinds yield `Message::Unknown`.
            pub fn decode(buf: &[u8]) -> Result<Message, ProtoError> {
                if buf.len() < 2 {
                    return Err(ProtoError::Short);
                }
                let kind = u16::from_be_bytes([buf[0], buf[1]]);
                let body = &buf[2..];
                Ok(match kind {
                    $($kind => Message::$variant(postcard::from_bytes(body)?),)*
                    other => Message::Unknown { kind: other },
                })
            }
        }
    };
}

messages! {
    1  => Hello(Hello),
    2  => Ping(Ping),
    3  => Pong(Pong),
    4  => PairConfirm(PairConfirm),
    10 => ClipText(ClipText),
    11 => ClipOffer(ClipOffer),
    12 => ClipPull(ClipPull),
    13 => ClipBlob(ClipBlob),
    20 => Notification(Notification),
    21 => Dnd(Dnd),
    30 => KvmEnter(KvmEnter),
    31 => KvmLeave(KvmLeave),
    32 => KvmReturn(KvmReturn),
    33 => MouseMove(MouseMove),
    34 => MouseButton(MouseButtonEvent),
    35 => Scroll(Scroll),
    36 => Key(KeyEvent),
    37 => KvmReady(KvmReady),
    38 => KvmAck(KvmAck),
    39 => KvmHeartbeat(KvmHeartbeat),
    40 => CamStart(CamStart),
    41 => CamStop(CamStop),
    42 => CamStatus(CamStatus),
    43 => CamFeedback(CamFeedback),
    44 => KvmError(KvmError),
    50 => AudioStart(AudioStart),
    51 => AudioStop(AudioStop),
    52 => AudioStatus(AudioStatus),
    53 => AudioFeedback(AudioFeedback),
}

/// Encode a message with its `u32` big-endian length prefix, ready to be
/// written to a stream.
pub fn encode_frame(msg: &Message) -> Result<Vec<u8>, ProtoError> {
    let body = msg.encode()?;
    if body.len() > MAX_FRAME_LEN {
        return Err(ProtoError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Validate a frame length read from the wire.
pub fn check_frame_len(len: u32) -> Result<usize, ProtoError> {
    let len = len as usize;
    if len < 2 {
        return Err(ProtoError::Short);
    }
    if len > MAX_FRAME_LEN {
        return Err(ProtoError::TooLarge(len));
    }
    Ok(len)
}

/// Is a peer with protocol `v` compatible with us?
pub fn version_compatible(v: u32) -> bool {
    v >= MIN_PROTO_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: Message) {
        let enc = m.encode().unwrap();
        let dec = Message::decode(&enc).unwrap();
        assert_eq!(m, dec);
        let framed = encode_frame(&m).unwrap();
        let len = u32::from_be_bytes(framed[..4].try_into().unwrap());
        assert_eq!(check_frame_len(len).unwrap(), framed.len() - 4);
        assert_eq!(Message::decode(&framed[4..]).unwrap(), m);
    }

    #[test]
    fn roundtrip_all_kinds() {
        roundtrip(Message::Hello(Hello {
            proto_version: PROTO_VERSION,
            device_id: "abc".into(),
            device_name: "mac".into(),
            device_type: "laptop".into(),
            features: vec![Feature::Clipboard, Feature::CameraSource],
        }));
        roundtrip(Message::Ping(Ping { seq: 7, t_us: 1 }));
        roundtrip(Message::Pong(Pong {
            seq: 7,
            echo_t_us: 1,
            t_us: 2,
        }));
        roundtrip(Message::PairConfirm(PairConfirm { accepted: true }));
        roundtrip(Message::ClipText(ClipText {
            hash: [3; 32],
            text: "héllo ✓".into(),
            html: Some("<b>x</b>".into()),
        }));
        roundtrip(Message::ClipOffer(ClipOffer {
            id: 9,
            hash: [1; 32],
            kind: ClipKind::Files,
            total_size: 10,
            items: vec![ClipItemMeta {
                name: "a.txt".into(),
                size: 10,
            }],
        }));
        roundtrip(Message::ClipPull(ClipPull { id: 9 }));
        roundtrip(Message::ClipBlob(ClipBlob {
            id: 9,
            available: true,
            items: vec![BlobItem {
                name: "a".into(),
                wire_size: 4,
                raw_size: 10,
                zstd: true,
            }],
        }));
        roundtrip(Message::Notification(Notification {
            app: "Mail".into(),
            title: "t".into(),
            subtitle: "".into(),
            body: "b".into(),
            timestamp_ms: 5,
        }));
        roundtrip(Message::Dnd(Dnd {
            enabled: true,
            changed_at_ms: 3,
        }));
        roundtrip(Message::KvmEnter(KvmEnter {
            edge: Edge::Left,
            ratio: 0.5,
            modifiers: modifiers::SHIFT,
        }));
        roundtrip(Message::KvmLeave(KvmLeave));
        roundtrip(Message::KvmReturn(KvmReturn { ratio: 0.25 }));
        roundtrip(Message::KvmReady(KvmReady { ready: true }));
        roundtrip(Message::KvmAck(KvmAck));
        roundtrip(Message::KvmHeartbeat(KvmHeartbeat));
        roundtrip(Message::KvmError(KvmError {
            reason: "Accessibility permission missing".into(),
        }));
        roundtrip(Message::MouseMove(MouseMove { dx: -4, dy: 1000 }));
        roundtrip(Message::MouseButton(MouseButtonEvent {
            button: MouseButton::Right,
            pressed: true,
        }));
        roundtrip(Message::Scroll(Scroll { dx: 0, dy: -3 }));
        roundtrip(Message::Key(KeyEvent {
            hid: 0x04,
            pressed: false,
        }));
        roundtrip(Message::CamStart(CamStart {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 4000,
        }));
        roundtrip(Message::CamStop(CamStop));
        roundtrip(Message::CamStatus(CamStatus {
            state: CamState::Busy,
            detail: "in use".into(),
        }));
        roundtrip(Message::CamFeedback(CamFeedback {
            loss_permille: 12,
            latency_ms: 60,
            fps_received: 30,
            request_keyframe: true,
        }));
        roundtrip(Message::AudioStart(AudioStart {
            stream: AudioStream::Mic,
            bitrate_kbps: 0,
            min_bitrate_kbps: 64,
            max_bitrate_kbps: 256,
            frame_ms_x10: 50,
            redundancy: 2,
            complexity: 8,
        }));
        roundtrip(Message::AudioStop(AudioStop {
            stream: AudioStream::Speaker,
        }));
        roundtrip(Message::AudioStatus(AudioStatus {
            stream: AudioStream::Mic,
            state: CamState::Unavailable,
            detail: "microphone permission denied".into(),
            sample_rate: 44_100,
            bitrate_kbps: 0,
        }));
        roundtrip(Message::AudioFeedback(AudioFeedback {
            stream: AudioStream::Speaker,
            plc_permille: 1,
            loss_permille: 12,
            buffer_ms: 9,
            target_ms: 8,
            peak_gap_ms: 14,
            underruns: 0,
            latency_ms: 21,
        }));
    }

    #[test]
    fn audio_kinds_are_stable() {
        assert_eq!(
            Message::AudioStop(AudioStop {
                stream: AudioStream::Mic
            })
            .kind(),
            51
        );
        assert_eq!(
            Message::AudioFeedback(AudioFeedback {
                stream: AudioStream::Mic,
                plc_permille: 0,
                loss_permille: 0,
                buffer_ms: 0,
                target_ms: 0,
                peak_gap_ms: 0,
                underruns: 0,
                latency_ms: 0,
            })
            .kind(),
            53
        );
    }

    #[test]
    fn unknown_kind_is_ignored_not_an_error() {
        // A newer peer sends kind 999 with some body we can't parse.
        let mut buf = 999u16.to_be_bytes().to_vec();
        buf.extend_from_slice(&[1, 2, 3, 4, 5]);
        assert_eq!(
            Message::decode(&buf).unwrap(),
            Message::Unknown { kind: 999 }
        );
    }

    #[test]
    fn kind_tags_are_stable() {
        // These numbers are part of the wire contract; changing them breaks
        // compatibility with released peers.
        assert_eq!(Message::Ping(Ping { seq: 0, t_us: 0 }).kind(), 2);
        assert_eq!(
            Message::ClipText(ClipText {
                hash: [0; 32],
                text: String::new(),
                html: None
            })
            .kind(),
            10
        );
        assert_eq!(
            Message::Key(KeyEvent {
                hid: 0,
                pressed: true
            })
            .kind(),
            36
        );
        assert_eq!(
            Message::CamFeedback(CamFeedback {
                loss_permille: 0,
                latency_ms: 0,
                fps_received: 0,
                request_keyframe: false
            })
            .kind(),
            43
        );
    }

    #[test]
    fn trailing_bytes_from_newer_peer_are_tolerated() {
        // Older decoder reading a payload a newer peer extended at the end.
        let mut enc = Message::Dnd(Dnd {
            enabled: true,
            changed_at_ms: 1,
        })
        .encode()
        .unwrap();
        enc.extend_from_slice(&[0xAA, 0xBB]);
        assert_eq!(
            Message::decode(&enc).unwrap(),
            Message::Dnd(Dnd {
                enabled: true,
                changed_at_ms: 1
            })
        );
    }

    #[test]
    fn frame_length_bounds() {
        assert!(check_frame_len(0).is_err());
        assert!(check_frame_len(1).is_err());
        assert!(check_frame_len(2).is_ok());
        assert!(check_frame_len((MAX_FRAME_LEN + 1) as u32).is_err());
    }

    #[test]
    fn truncated_body_errors() {
        let enc = Message::Ping(Ping {
            seq: 300,
            t_us: 1 << 40,
        })
        .encode()
        .unwrap();
        assert!(Message::decode(&enc[..3]).is_err());
        assert!(Message::decode(&enc[..1]).is_err());
    }

    #[test]
    fn version_gate() {
        assert!(version_compatible(PROTO_VERSION));
        assert!(version_compatible(PROTO_VERSION + 5));
        assert!(!version_compatible(0));
    }
}
