//! A live link to the peer over one QUIC connection.
//!
//! Layout of the connection:
//!
//! * **Control stream** — the first bidirectional stream, opened by the
//!   dialer. Carries `Hello`, heartbeats, and every small control message
//!   (clipboard text, notifications, DND, KVM events, camera control), in
//!   order, as length-prefixed frames.
//! * **Bulk streams** — further bidirectional streams, one per heavy
//!   clipboard pull, so a large image never delays a keystroke.
//! * **Datagrams** — camera video fragments and audio packets (unreliable;
//!   late = dropped), demultiplexed by their first byte into separate queues
//!   so audio never waits behind video.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use bytes::Bytes;
use linker_platform::now_us;
use linker_proto::{check_frame_len, encode_frame, Feature, Hello, Message, Ping, Pong};
use quinn::{Connection, RecvStream, SendStream};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Declared dead after this long without hearing anything (3 missed beats).
pub const HEARTBEAT_DEAD: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Read one framed message. `Ok(None)` on clean end-of-stream.
pub async fn read_msg(recv: &mut RecvStream) -> anyhow::Result<Option<Message>> {
    let mut len = [0u8; 4];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = check_frame_len(u32::from_be_bytes(len))?;
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(Some(Message::decode(&buf)?))
}

pub async fn write_msg(send: &mut SendStream, msg: &Message) -> anyhow::Result<()> {
    let frame = encode_frame(msg)?;
    send.write_all(&frame).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Clock sync (for camera latency measurement)
// ---------------------------------------------------------------------------

/// NTP-style estimate of the peer clock offset from ping/pong samples; keeps
/// the sample with the lowest RTT out of the last few.
#[derive(Default)]
pub struct ClockSync {
    samples: Mutex<VecDeque<(u64, i64)>>,
    offset_us: AtomicI64,
    rtt_us: AtomicU64,
}

impl ClockSync {
    /// `t0` = our send time, `t1` = peer time, `t2` = our receive time.
    pub fn add_sample(&self, t0: u64, t1: u64, t2: u64) {
        if t2 < t0 {
            return;
        }
        let rtt = t2 - t0;
        let offset = t1 as i64 - (t0 as i64 + (rtt / 2) as i64);
        let mut s = self.samples.lock().expect("clock lock");
        s.push_back((rtt, offset));
        while s.len() > 8 {
            s.pop_front();
        }
        if let Some(&(best_rtt, best_off)) = s.iter().min_by_key(|(r, _)| *r) {
            self.rtt_us.store(best_rtt, Ordering::Relaxed);
            self.offset_us.store(best_off, Ordering::Relaxed);
        }
    }

    /// Peer clock minus our clock, µs.
    pub fn offset_us(&self) -> i64 {
        self.offset_us.load(Ordering::Relaxed)
    }

    pub fn rtt_us(&self) -> u64 {
        self.rtt_us.load(Ordering::Relaxed)
    }

    /// Convert a peer timestamp to our clock.
    pub fn peer_to_local(&self, peer_us: u64) -> u64 {
        (peer_us as i64 - self.offset_us()).max(0) as u64
    }
}

// ---------------------------------------------------------------------------
// Link handle
// ---------------------------------------------------------------------------

/// Cheap, clonable handle to the live link used by feature modules.
#[derive(Clone)]
pub struct Link {
    pub session_id: u64,
    pub conn: Connection,
    pub peer: Arc<Hello>,
    ctl: mpsc::Sender<Message>,
    pub clock: Arc<ClockSync>,
    pub since: Instant,
}

impl Link {
    /// Queue a control message without waiting. Returns false if the link is
    /// gone or badly congested.
    pub fn send(&self, msg: Message) -> bool {
        match self.ctl.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(m)) => {
                tracing::warn!(kind = m.kind(), "control queue full; dropping message");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Queue a control message, waiting for room.
    pub async fn send_wait(&self, msg: Message) -> bool {
        self.ctl.send(msg).await.is_ok()
    }

    pub fn peer_has(&self, f: Feature) -> bool {
        self.peer.features.contains(&f)
    }

    pub fn max_datagram(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    pub fn send_datagram(&self, data: Bytes) -> bool {
        self.conn.send_datagram(data).is_ok()
    }

    pub async fn open_bulk(&self) -> anyhow::Result<(SendStream, RecvStream)> {
        Ok(self.conn.open_bi().await?)
    }
}

/// Where incoming messages go. Each feature owns one receiver.
#[derive(Clone)]
pub struct Router {
    pub clipboard: mpsc::Sender<Message>,
    pub clipboard_bulk: mpsc::Sender<(SendStream, RecvStream)>,
    pub notify: mpsc::Sender<Message>,
    pub kvm: mpsc::Sender<Message>,
    pub camera: mpsc::Sender<Message>,
    pub camera_dgram: mpsc::Sender<Bytes>,
    pub audio: mpsc::Sender<Message>,
    pub audio_dgram: mpsc::Sender<Bytes>,
}

pub struct RouterRx {
    pub clipboard: mpsc::Receiver<Message>,
    pub clipboard_bulk: mpsc::Receiver<(SendStream, RecvStream)>,
    pub notify: mpsc::Receiver<Message>,
    pub kvm: mpsc::Receiver<Message>,
    pub camera: mpsc::Receiver<Message>,
    pub camera_dgram: mpsc::Receiver<Bytes>,
    pub audio: mpsc::Receiver<Message>,
    pub audio_dgram: mpsc::Receiver<Bytes>,
}

pub fn router() -> (Router, RouterRx) {
    let (a, ar) = mpsc::channel(256);
    let (b, br) = mpsc::channel(8);
    let (c, cr) = mpsc::channel(256);
    let (d, dr) = mpsc::channel(4096);
    let (e, er) = mpsc::channel(64);
    let (f, fr) = mpsc::channel(2048);
    let (g, gr) = mpsc::channel(64);
    // ~200 packets/s per stream at 5 ms frames: ~2.5 s of headroom.
    let (h, hr) = mpsc::channel(512);
    (
        Router {
            clipboard: a,
            clipboard_bulk: b,
            notify: c,
            kvm: d,
            camera: e,
            camera_dgram: f,
            audio: g,
            audio_dgram: h,
        },
        RouterRx {
            clipboard: ar,
            clipboard_bulk: br,
            notify: cr,
            kvm: dr,
            camera: er,
            camera_dgram: fr,
            audio: gr,
            audio_dgram: hr,
        },
    )
}

// ---------------------------------------------------------------------------
// Handshake + session
// ---------------------------------------------------------------------------

/// Exchange `Hello` on the control stream. The dialer opens the stream.
pub async fn handshake(
    conn: &Connection,
    we_dialed: bool,
    hello: &Hello,
) -> anyhow::Result<(Hello, SendStream, RecvStream)> {
    let fut = async {
        let (mut send, mut recv) = if we_dialed {
            conn.open_bi().await?
        } else {
            conn.accept_bi().await?
        };
        write_msg(&mut send, &Message::Hello(hello.clone())).await?;
        let peer = match read_msg(&mut recv).await? {
            Some(Message::Hello(h)) => h,
            Some(other) => bail!("expected Hello, got kind {}", other.kind()),
            None => bail!("peer closed during handshake"),
        };
        if !linker_proto::version_compatible(peer.proto_version) {
            bail!("peer protocol version {} is too old", peer.proto_version);
        }
        anyhow::Ok((peer, send, recv))
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, fut)
        .await
        .context("handshake timed out")?
}

#[derive(Debug)]
pub enum EndReason {
    HeartbeatTimeout,
    Closed(String),
    Replaced,
}

impl Link {
    pub fn new(session_id: u64, conn: Connection, peer: Hello) -> (Link, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(4096);
        (
            Link {
                session_id,
                conn,
                peer: Arc::new(peer),
                ctl: tx,
                clock: Arc::new(ClockSync::default()),
                since: Instant::now(),
            },
            rx,
        )
    }
}

/// Aborts the wrapped tasks when dropped (also when the owning task is
/// itself aborted), so no helper outlives its session.
struct TaskGroup(Vec<tokio::task::JoinHandle<()>>);

trait ClearAndAbort {
    fn clear_and_abort(&mut self);
}

impl ClearAndAbort for Vec<tokio::task::JoinHandle<()>> {
    fn clear_and_abort(&mut self) {
        for t in self.drain(..) {
            t.abort();
        }
    }
}

impl Drop for TaskGroup {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

/// Drive a session until the link dies. Never returns early on a
/// recoverable per-message error.
pub async fn run_session(
    link: Link,
    mut send: SendStream,
    mut recv: RecvStream,
    mut ctl_rx: mpsc::Receiver<Message>,
    router: Router,
) -> EndReason {
    let conn = link.conn.clone();

    // Writer: serialises everything queued by features + heartbeat.
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if let Err(e) = write_msg(&mut send, &m).await {
                tracing::debug!("control write failed: {e:#}");
                break;
            }
            // Coalesce bursts (e.g. mouse motion) into fewer packets.
            while let Ok(m) = ctl_rx.try_recv() {
                if write_msg(&mut send, &m).await.is_err() {
                    return;
                }
            }
            let _ = send.flush().await;
        }
        let _ = send.finish();
    });

    // Bulk stream acceptor.
    let bulk = {
        let conn = conn.clone();
        let tx = router.clipboard_bulk.clone();
        tokio::spawn(async move {
            while let Ok(pair) = conn.accept_bi().await {
                if tx.send(pair).await.is_err() {
                    break;
                }
            }
        })
    };

    // Datagram receiver: audio and camera go to separate queues.
    let dgrams = {
        let conn = conn.clone();
        let cam = router.camera_dgram.clone();
        let audio = router.audio_dgram.clone();
        tokio::spawn(async move {
            while let Ok(d) = conn.read_datagram().await {
                // Never block the network on a slow sink: drop instead.
                if d.first()
                    .is_some_and(|b| linker_audio::proto::is_audio_tag(*b))
                {
                    let _ = audio.try_send(d);
                } else {
                    let _ = cam.try_send(d);
                }
            }
        })
    };

    let last_rx = Arc::new(Mutex::new(Instant::now()));

    // Heartbeat sender + watchdog (a plain future, so it dies with the session).
    let heartbeat = {
        let link = link.clone();
        let last_rx = last_rx.clone();
        async move {
            let mut seq = 0u64;
            let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let silent = last_rx.lock().expect("lock").elapsed();
                if silent > HEARTBEAT_DEAD {
                    return;
                }
                seq += 1;
                link.send(Message::Ping(Ping {
                    seq,
                    t_us: now_us(),
                }));
            }
        }
    };

    let reader = async {
        loop {
            match read_msg(&mut recv).await {
                Ok(Some(msg)) => {
                    *last_rx.lock().expect("lock") = Instant::now();
                    dispatch(&link, &router, msg).await;
                }
                Ok(None) => return EndReason::Closed("peer closed control stream".into()),
                Err(e) => return EndReason::Closed(format!("{e:#}")),
            }
        }
    };

    let mut tasks = TaskGroup(vec![writer, bulk, dgrams]);
    let reason = tokio::select! {
        r = reader => r,
        _ = heartbeat => EndReason::HeartbeatTimeout,
        e = conn.closed() => EndReason::Closed(e.to_string()),
    };
    tasks.0.clear_and_abort();
    conn.close(0u32.into(), b"session ended");
    reason
}

async fn dispatch(link: &Link, router: &Router, msg: Message) {
    match msg {
        Message::Ping(p) => {
            link.send(Message::Pong(Pong {
                seq: p.seq,
                echo_t_us: p.t_us,
                t_us: now_us(),
            }));
        }
        Message::Pong(p) => link.clock.add_sample(p.echo_t_us, p.t_us, now_us()),
        Message::Hello(_) | Message::PairConfirm(_) => {}
        m @ (Message::ClipText(_) | Message::ClipOffer(_)) => {
            let _ = router.clipboard.send(m).await;
        }
        m @ (Message::Notification(_) | Message::Dnd(_)) => {
            let _ = router.notify.send(m).await;
        }
        m @ (Message::KvmEnter(_)
        | Message::KvmLeave(_)
        | Message::KvmReturn(_)
        | Message::KvmReady(_)
        | Message::KvmAck(_)
        | Message::KvmHeartbeat(_)
        | Message::KvmError(_)
        | Message::MouseMove(_)
        | Message::MouseButton(_)
        | Message::Scroll(_)
        | Message::Key(_)) => {
            let _ = router.kvm.send(m).await;
        }
        m @ (Message::CamStart(_)
        | Message::CamStop(_)
        | Message::CamStatus(_)
        | Message::CamFeedback(_)) => {
            let _ = router.camera.send(m).await;
        }
        m @ (Message::AudioStart(_)
        | Message::AudioStop(_)
        | Message::AudioStatus(_)
        | Message::AudioFeedback(_)) => {
            let _ = router.audio.send(m).await;
        }
        Message::ClipPull(_) | Message::ClipBlob(_) => {
            tracing::debug!("bulk message on control stream ignored");
        }
        Message::Unknown { kind } => {
            tracing::debug!(kind, "ignoring unknown message kind from newer peer")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_offset_estimate() {
        let c = ClockSync::default();
        // Peer clock is 1_000_000 µs ahead; RTT 2000 µs.
        c.add_sample(10_000, 1_011_000, 12_000);
        assert_eq!(c.offset_us(), 1_000_000);
        assert_eq!(c.rtt_us(), 2000);
        // A noisier sample (larger RTT) doesn't override the better one.
        c.add_sample(20_000, 1_030_000, 40_000);
        assert_eq!(c.offset_us(), 1_000_000);
        assert_eq!(c.peer_to_local(1_050_000), 50_000);
    }
}
