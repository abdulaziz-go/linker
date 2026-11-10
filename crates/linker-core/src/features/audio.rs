//! Audio sharing over the link (see `docs/feature-audio.md`).
//!
//! Two independent streams, each with a **sending** side (capture → Opus →
//! redundancy packets → QUIC datagrams) and a **receiving** side (reorder →
//! decode/PLC → adaptive jitter buffer → device), using the engine ported
//! from audiobridge (`linker-audio`):
//!
//! | stream  | source role (Mac)                | sink role (Ubuntu)                         |
//! |---------|----------------------------------|--------------------------------------------|
//! | mic     | send: default mic, on request    | recv: virtual source "Linker Microphone"   |
//! | speaker | recv: default output, on audio   | send: virtual sink "Linker Speaker"        |
//!
//! The receiving side requests a stream (`AudioStart`, with its own codec
//! settings) and reports quality once a second (`AudioFeedback`), which the
//! sender uses for automatic bitrate. Each side runs its packet work on a
//! dedicated thread so the async runtime never sits between the network and
//! the audio clock.

use std::sync::mpsc::{self as stdmpsc, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use linker_audio::frame::dbfs;
use linker_audio::jitter::JitterBuffer;
use linker_audio::proto::{self, TAG_MIC, TAG_SPEAKER};
use linker_audio::receiver::{Receiver, ReceiverStats, TICK};
use linker_audio::sender::{BitrateControl, Sender, SenderParams};
use linker_audio::sim::{Impair, NetSim};
use linker_platform::{
    now_us, AudioEvent, AudioSpec, AudioStream as Device, CamState, PcmSink, PcmSource,
};
use linker_proto::{AudioFeedback, AudioStart, AudioStatus, AudioStop, AudioStream, Message};
use tokio::sync::mpsc;

use crate::config::{AudioConfig, AudioMode};
use crate::link::Link;
use crate::status::AudioStats;
use crate::Shared;

/// Env var with a network impairment spec for the *sending* side, e.g.
/// `loss=5,reorder=10,jitter=5,stall_every=1500,stall_ms=100` (field tests).
pub const IMPAIR_ENV: &str = "LINKER_AUDIO_IMPAIR";
/// Stop the Mac microphone this long after the last Ubuntu reader left.
const MIC_IDLE_STOP: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(2);
/// Opus RESTRICTED_LOWDELAY algorithmic delay.
const CODEC_DELAY_MS: f64 = 2.5;

pub fn tag_of(s: AudioStream) -> u8 {
    match s {
        AudioStream::Mic => TAG_MIC,
        AudioStream::Speaker => TAG_SPEAKER,
    }
}

/// Our request for a stream we receive, from the local config.
pub fn start_request(cfg: &AudioConfig, stream: AudioStream) -> AudioStart {
    AudioStart {
        stream,
        bitrate_kbps: cfg.bitrate_kbps,
        min_bitrate_kbps: cfg.min_bitrate_kbps,
        max_bitrate_kbps: cfg.max_bitrate_kbps,
        frame_ms_x10: (cfg.frame_ms * 10.0).round() as u16,
        redundancy: cfg.redundancy,
        complexity: cfg.complexity,
    }
}

/// Sanitise a peer's request into encoder parameters.
pub fn sender_params(req: &AudioStart, max_datagram: usize) -> (SenderParams, BitrateControl) {
    let frame_ms = match req.frame_ms_x10 {
        25 => 2.5,
        100 => 10.0,
        200 => 20.0,
        _ => 5.0,
    };
    let (min, max) = (
        req.min_bitrate_kbps.clamp(6, 510),
        req.max_bitrate_kbps.clamp(6, 510),
    );
    let rate = BitrateControl::new(req.bitrate_kbps.min(510), min, max);
    (
        SenderParams {
            bitrate_bps: rate.kbps as i32 * 1000,
            complexity: req.complexity.min(10) as i32,
            frame_ms,
            redundancy: req.redundancy.min(3) as usize,
            max_datagram,
        },
        rate,
    )
}

// ---------------------------------------------------------------------------
// Sending side: capture → Sender thread → datagrams
// ---------------------------------------------------------------------------

enum TxCmd {
    Pcm(Vec<i16>, u64),
    Start(SenderParams),
    Stop,
    Link(Option<Link>),
    Bitrate(u32),
}

#[derive(Default)]
struct TxShared {
    packets: u64,
    bytes: u64,
    peak: u16,
    dropped_pcm: u64,
    running: bool,
    bitrate_kbps: u32,
}

fn tx_thread(tag: u8, rx: stdmpsc::Receiver<TxCmd>, shared: Arc<Mutex<TxShared>>) {
    let mut sender: Option<Sender> = None;
    let mut link: Option<Link> = None;
    let impair = std::env::var(IMPAIR_ENV)
        .ok()
        .and_then(|s| match Impair::parse(&s) {
            Ok(i) if !i.is_none() => {
                tracing::warn!(spec = %s, "audio: simulating network impairment on send");
                Some(i)
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("audio: ignoring {IMPAIR_ENV}: {e}");
                None
            }
        });
    let mut sim = impair.map(NetSim::new);
    let mut since_mtu_check = 0u32;
    loop {
        let timeout = match sim.as_ref().and_then(|s| s.next_due()) {
            Some(due) => due.saturating_duration_since(Instant::now()),
            None => Duration::from_millis(50),
        };
        match rx.recv_timeout(timeout) {
            Ok(TxCmd::Pcm(pcm, ts)) => {
                let (Some(s), Some(l)) = (sender.as_mut(), link.as_ref()) else {
                    continue;
                };
                since_mtu_check += 1;
                if since_mtu_check >= 200 {
                    since_mtu_check = 0;
                    if let Some(m) = l.max_datagram() {
                        if m != s.params().max_datagram {
                            s.set_max_datagram(m);
                        }
                    }
                }
                let now = Instant::now();
                s.push(&pcm, ts, |d| match sim.as_mut() {
                    Some(sim) => sim.send(now, d),
                    None => {
                        l.send_datagram(Bytes::copy_from_slice(d));
                    }
                });
                let st = s.stats();
                let mut sh = shared.lock().expect("tx lock");
                sh.packets = st.packets;
                sh.bytes = st.bytes;
                sh.peak = sh.peak.max(s.take_peak());
            }
            Ok(TxCmd::Start(p)) => {
                // A new request always starts a fresh stream (new seq, new
                // encoder) — the receiver detects the restart.
                match Sender::new(tag, p) {
                    Ok(s) => {
                        sender = Some(s);
                        let mut sh = shared.lock().expect("tx lock");
                        sh.running = true;
                        sh.bitrate_kbps = (p.bitrate_bps / 1000) as u32;
                    }
                    Err(e) => tracing::warn!("audio encoder: {e}"),
                }
            }
            Ok(TxCmd::Stop) => {
                sender = None;
                shared.lock().expect("tx lock").running = false;
            }
            Ok(TxCmd::Link(l)) => link = l,
            Ok(TxCmd::Bitrate(kbps)) => {
                if let Some(s) = sender.as_mut() {
                    if s.set_bitrate(kbps as i32 * 1000).is_ok() {
                        shared.lock().expect("tx lock").bitrate_kbps = kbps;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let (Some(sim), Some(l)) = (sim.as_mut(), link.as_ref()) {
            sim.poll(Instant::now(), |d| {
                l.send_datagram(Bytes::copy_from_slice(d));
            });
        }
    }
}

/// How the local device of a sending stream is managed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TxDevice {
    /// Physical capture opened only while the peer requests the stream (Mac mic).
    CaptureOnRequest,
    /// Virtual device kept for the daemon's lifetime (Ubuntu "Linker Speaker").
    Virtual,
}

struct TxStream {
    stream: AudioStream,
    kind: TxDevice,
    cmd: SyncSender<TxCmd>,
    shared: Arc<Mutex<TxShared>>,
    device: Option<Box<dyn Device>>,
    ev_tx: mpsc::Sender<AudioEvent>,
    requested: Option<AudioStart>,
    /// Link session the request arrived on (a request may be processed
    /// before this task sees the link-change event of its own session).
    requested_session: Option<u64>,
    rate: BitrateControl,
    params: Option<SenderParams>,
    retry_at: Option<Instant>,
    state: String,
    peer_fb: Option<AudioFeedback>,
    last_packets: u64,
    last_bytes: u64,
}

impl TxStream {
    fn new(stream: AudioStream, kind: TxDevice, ev_tx: mpsc::Sender<AudioEvent>) -> TxStream {
        let (cmd, rx) = stdmpsc::sync_channel(256);
        let shared = Arc::new(Mutex::new(TxShared::default()));
        let sh = shared.clone();
        std::thread::Builder::new()
            .name(format!("linker-audio-tx-{}", stream.name()))
            .spawn(move || tx_thread(tag_of(stream), rx, sh))
            .expect("spawn audio tx thread");
        TxStream {
            stream,
            kind,
            cmd,
            shared,
            device: None,
            ev_tx,
            requested: None,
            requested_session: None,
            rate: BitrateControl::new(0, 64, 256),
            params: None,
            retry_at: None,
            state: "idle".into(),
            peer_fb: None,
            last_packets: 0,
            last_bytes: 0,
        }
    }

    fn pcm_sink(&self) -> PcmSink {
        let cmd = self.cmd.clone();
        let shared = self.shared.clone();
        Box::new(move |pcm: &[i16]| {
            if let Err(TrySendError::Full(_)) = cmd.try_send(TxCmd::Pcm(pcm.to_vec(), now_us())) {
                if let Ok(mut s) = shared.try_lock() {
                    s.dropped_pcm += 1;
                }
            }
        })
    }

    fn open(&mut self, shared: &Shared) -> Result<(), String> {
        let cfg = &shared.cfg.audio;
        let spec = match self.kind {
            TxDevice::CaptureOnRequest => AudioSpec {
                device: cfg.input_device.clone(),
                description: String::new(),
                period_ms: cfg.period_ms,
            },
            TxDevice::Virtual => AudioSpec {
                device: Some(cfg.speaker_node.clone()),
                description: cfg.speaker_name.clone(),
                period_ms: cfg.period_ms,
            },
        };
        let sink = self.pcm_sink();
        let ev = self.ev_tx.clone();
        let backend = shared.backend.clone();
        let kind = self.kind;
        // May block (permission prompt, device start).
        let r = tokio::task::block_in_place(move || match kind {
            TxDevice::CaptureOnRequest => backend.audio_capture(spec, sink, ev),
            TxDevice::Virtual => backend.virtual_speaker(spec, sink, ev),
        });
        match r {
            Ok(d) => {
                let i = d.info();
                tracing::info!(stream = self.stream.name(), device = %i.device, rate = i.sample_rate, "audio device open (send)");
                self.device = Some(d);
                self.retry_at = None;
                Ok(())
            }
            Err(e) => Err(e.0),
        }
    }

    fn sample_rate(&self) -> u32 {
        self.device.as_ref().map_or(0, |d| d.info().sample_rate)
    }

    fn send_status(&self, link: &Option<Link>, state: CamState, detail: String) {
        if let Some(l) = link {
            l.send(Message::AudioStatus(AudioStatus {
                stream: self.stream,
                state,
                detail,
                sample_rate: self.sample_rate(),
                bitrate_kbps: if state == CamState::Streaming {
                    self.rate.kbps
                } else {
                    0
                },
            }));
        }
    }

    /// Peer asked for the stream.
    fn on_start(&mut self, shared: &Shared, req: AudioStart, enabled: bool) {
        let link = shared.link();
        if !enabled {
            self.send_status(
                &link,
                CamState::Unavailable,
                format!(
                    "{} sharing is disabled on {}",
                    self.stream.name(),
                    shared.identity.device_name
                ),
            );
            return;
        }
        let max_dgram = link.as_ref().and_then(|l| l.max_datagram());
        let Some(max_dgram) = max_dgram else {
            self.send_status(
                &link,
                CamState::Unavailable,
                "peer does not accept datagrams".into(),
            );
            return;
        };
        let (params, rate) = sender_params(&req, max_dgram);
        tracing::info!(
            stream = self.stream.name(),
            ?params,
            "audio stream requested"
        );
        self.requested = Some(req);
        self.requested_session = link.as_ref().map(|l| l.session_id);
        self.rate = rate;
        self.params = Some(params);
        let _ = self.cmd.try_send(TxCmd::Link(link.clone()));
        let _ = self.cmd.try_send(TxCmd::Start(params));
        if self.device.is_none() {
            if let Err(e) = self.open(shared) {
                tracing::warn!(stream = self.stream.name(), "audio device unavailable: {e}");
                self.state = format!("unavailable ({e})");
                self.send_status(&link, CamState::Unavailable, e);
                // Keep trying while the stream is wanted (a busy device or
                // an AirPods switch is usually transient); tick() stops
                // retrying once the request is withdrawn.
                self.retry_at = Some(Instant::now() + RETRY);
                return;
            }
        }
        self.state = "streaming".into();
        self.send_status(&link, CamState::Streaming, String::new());
    }

    fn on_stop(&mut self) {
        self.requested = None;
        self.requested_session = None;
        self.params = None;
        let _ = self.cmd.try_send(TxCmd::Stop);
        if self.kind == TxDevice::CaptureOnRequest && self.device.take().is_some() {
            tracing::info!(stream = self.stream.name(), "audio capture stopped");
        }
        self.state = "idle".into();
        self.peer_fb = None;
    }

    fn on_link(&mut self, link: Option<Link>) {
        // A new session must re-request; the old one is gone either way —
        // unless the request already came on this very session (the peer
        // asks as soon as its side of the link is up, which can be before
        // our watch fires).
        let same = link.as_ref().map(|l| l.session_id);
        if self.requested.is_none() || self.requested_session != same || same.is_none() {
            self.on_stop();
        }
        let _ = self.cmd.try_send(TxCmd::Link(link));
    }

    fn on_feedback(&mut self, shared: &Shared, fb: AudioFeedback) {
        if self.requested.is_none() {
            return;
        }
        if let Some(kbps) = self
            .rate
            .on_feedback(fb.loss_permille, fb.plc_permille, fb.underruns)
        {
            tracing::debug!(stream = self.stream.name(), kbps, "audio bitrate adjusted");
            let _ = self.cmd.try_send(TxCmd::Bitrate(kbps));
            if self.device.is_some() {
                // Let the receiver show the current bitrate.
                self.send_status(&shared.link(), CamState::Streaming, String::new());
            }
        }
        self.peer_fb = Some(fb);
    }

    fn on_device_failed(&mut self, shared: &Shared, why: String) {
        tracing::warn!(stream = self.stream.name(), "audio device failed: {why}");
        self.device = None;
        self.state = format!("reopening ({why})");
        if self.requested.is_some() {
            self.send_status(&shared.link(), CamState::Busy, why);
        }
        self.retry_at = Some(Instant::now() + Duration::from_millis(500));
    }

    /// Periodic: reopen a failed device when it's needed.
    fn tick(&mut self, shared: &Shared) {
        let Some(at) = self.retry_at else { return };
        if Instant::now() < at {
            return;
        }
        let wanted = self.kind == TxDevice::Virtual || self.requested.is_some();
        if !wanted {
            self.retry_at = None;
            return;
        }
        match self.open(shared) {
            Ok(()) => {
                if self.requested.is_some() {
                    self.state = "streaming".into();
                    self.send_status(&shared.link(), CamState::Streaming, String::new());
                } else {
                    self.state = "idle".into();
                }
            }
            Err(e) => {
                self.state = format!("unavailable ({e})");
                self.retry_at = Some(Instant::now() + RETRY);
            }
        }
    }

    fn stats(&mut self, window: Duration) -> AudioStats {
        let (packets, bytes, peak, bitrate) = {
            let mut s = self.shared.lock().expect("tx lock");
            let p = std::mem::take(&mut s.peak);
            (s.packets, s.bytes, p, s.bitrate_kbps)
        };
        let secs = window.as_secs_f64().max(0.001);
        let (dp, db) = (
            packets.saturating_sub(self.last_packets),
            bytes.saturating_sub(self.last_bytes),
        );
        self.last_packets = packets;
        self.last_bytes = bytes;
        let info = self.device.as_ref().map(|d| d.info()).unwrap_or_default();
        let cb = self
            .device
            .as_ref()
            .map_or(0.0, |d| d.take_max_callback_gap_ms());
        let fb = self.peer_fb.unwrap_or(AudioFeedback {
            stream: self.stream,
            plc_permille: 0,
            loss_permille: 0,
            buffer_ms: 0,
            target_ms: 0,
            peak_gap_ms: 0,
            underruns: 0,
            latency_ms: 0,
        });
        let p = self.params.unwrap_or_default();
        AudioStats {
            stream: self.stream.name().into(),
            direction: "send".into(),
            state: self.state.clone(),
            device: info.device,
            sample_rate: info.sample_rate,
            bitrate_kbps: if self.requested.is_some() { bitrate } else { 0 },
            frame_ms: p.frame_ms,
            redundancy: p.redundancy as u8,
            buffer_ms: fb.buffer_ms as u32,
            target_ms: fb.target_ms as u32,
            peak_gap_ms: fb.peak_gap_ms as u32,
            plc: fb.plc_permille as u64,
            underruns: fb.underruns as u64,
            loss_permille: fb.loss_permille as u32,
            latency_ms: fb.latency_ms as f64,
            callback_gap_ms: cb,
            packets_per_s: dp as f64 / secs,
            kbps: db as f64 * 8.0 / secs / 1000.0,
            peak_dbfs: dbfs(peak),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Receiving side: datagrams → Receiver thread → jitter buffer ← device
// ---------------------------------------------------------------------------

enum RxCmd {
    Packet { data: Bytes, capture_local_us: u64 },
    Reset,
}

#[derive(Default)]
struct RxShared {
    stats: ReceiverStats,
    /// Network latency samples (µs, capture → arrival) since last report.
    net_lat_us: Vec<u64>,
    last_packet: Option<Instant>,
}

/// Port of the receiver main loop: packets + a 5 ms deadline that flushes
/// frames waiting on a hole.
fn rx_thread(
    rx: stdmpsc::Receiver<RxCmd>,
    mut receiver: Receiver,
    shared: Arc<Mutex<RxShared>>,
    audio_seen: mpsc::Sender<()>,
) {
    let mut idle = true;
    loop {
        match rx.recv_timeout(TICK) {
            Ok(RxCmd::Packet {
                data,
                capture_local_us,
            }) => {
                let now = Instant::now();
                let Some((_, _, pkt)) = proto::unwrap(&data) else {
                    continue;
                };
                if receiver.on_packet(pkt, now).is_err() {
                    continue;
                }
                let mut sh = shared.lock().expect("rx lock");
                if capture_local_us != 0 && sh.net_lat_us.len() < 4096 {
                    sh.net_lat_us
                        .push(now_us().saturating_sub(capture_local_us));
                }
                sh.last_packet = Some(now);
                sh.stats = receiver.stats();
                drop(sh);
                if idle {
                    idle = false;
                    let _ = audio_seen.try_send(());
                }
            }
            Ok(RxCmd::Reset) => {
                receiver.reset();
                idle = true;
            }
            Err(RecvTimeoutError::Timeout) => {
                receiver.on_tick(Instant::now());
                let mut sh = shared.lock().expect("rx lock");
                sh.stats = receiver.stats();
                if sh
                    .last_packet
                    .is_some_and(|t| t.elapsed() > Duration::from_secs(1))
                {
                    idle = true;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RxDevice {
    /// Physical output opened when audio arrives, closed when idle (Mac speaker).
    PlaybackOnAudio,
    /// Virtual device kept for the daemon's lifetime (Ubuntu "Linker Microphone").
    Virtual,
}

struct RxStream {
    stream: AudioStream,
    kind: RxDevice,
    jb: Arc<Mutex<JitterBuffer>>,
    cmd: SyncSender<RxCmd>,
    shared: Arc<Mutex<RxShared>>,
    device: Option<Box<dyn Device>>,
    ev_tx: mpsc::Sender<AudioEvent>,
    /// AudioStart sent on the current link.
    requesting: bool,
    /// Session the AudioStart went out on.
    requested_session: Option<u64>,
    demand_gone_since: Option<Instant>,
    retry_at: Option<Instant>,
    peer_state: String,
    sender_rate: u32,
    last: ReceiverStats,
    last_underruns: u64,
    request: AudioStart,
    period_ms: f64,
}

impl RxStream {
    fn new(
        stream: AudioStream,
        kind: RxDevice,
        cfg: &AudioConfig,
        ev_tx: mpsc::Sender<AudioEvent>,
        audio_seen: mpsc::Sender<()>,
    ) -> RxStream {
        let jb = Arc::new(Mutex::new(JitterBuffer::new(
            linker_platform::AUDIO_RATE,
            linker_platform::AUDIO_CHANNELS,
            cfg.min_buffer_ms,
            cfg.max_buffer_ms,
        )));
        let receiver = Receiver::new(jb.clone()).expect("opus decoder");
        let (cmd, rx) = stdmpsc::sync_channel(1024);
        let shared = Arc::new(Mutex::new(RxShared::default()));
        let sh = shared.clone();
        std::thread::Builder::new()
            .name(format!("linker-audio-rx-{}", stream.name()))
            .spawn(move || rx_thread(rx, receiver, sh, audio_seen))
            .expect("spawn audio rx thread");
        RxStream {
            stream,
            kind,
            jb,
            cmd,
            shared,
            device: None,
            ev_tx,
            requesting: false,
            requested_session: None,
            demand_gone_since: None,
            retry_at: None,
            peer_state: String::new(),
            sender_rate: 0,
            last: ReceiverStats::default(),
            last_underruns: 0,
            request: start_request(cfg, stream),
            period_ms: cfg.period_ms,
        }
    }

    fn pcm_source(&self) -> PcmSource {
        let jb = self.jb.clone();
        Box::new(move |out: &mut [i16]| {
            let now = Instant::now();
            match jb.lock() {
                Ok(mut j) => j.pull(out, now),
                Err(_) => out.fill(0),
            }
        })
    }

    fn open(&mut self, shared: &Shared) -> Result<(), String> {
        let cfg = &shared.cfg.audio;
        let spec = match self.kind {
            RxDevice::PlaybackOnAudio => AudioSpec {
                device: cfg.output_device.clone(),
                description: String::new(),
                period_ms: cfg.period_ms,
            },
            RxDevice::Virtual => AudioSpec {
                device: Some(cfg.mic_node.clone()),
                description: cfg.mic_name.clone(),
                period_ms: cfg.period_ms,
            },
        };
        let src = self.pcm_source();
        let ev = self.ev_tx.clone();
        let backend = shared.backend.clone();
        let kind = self.kind;
        let r = tokio::task::block_in_place(move || match kind {
            RxDevice::PlaybackOnAudio => backend.audio_playback(spec, src, ev),
            RxDevice::Virtual => backend.virtual_mic(spec, src, ev),
        });
        match r {
            Ok(d) => {
                let i = d.info();
                tracing::info!(stream = self.stream.name(), device = %i.device, rate = i.sample_rate, "audio device open (recv)");
                if i.period_frames > 0 {
                    self.period_ms = i.period_frames as f64 * 1000.0 / i.sample_rate.max(1) as f64;
                }
                self.device = Some(d);
                self.retry_at = None;
                Ok(())
            }
            Err(e) => Err(e.0),
        }
    }

    fn request(&mut self, link: &Link) {
        if !self.requesting {
            tracing::info!(
                stream = self.stream.name(),
                "requesting audio stream from peer"
            );
            // The sender starts every requested stream at seq 0: forget the
            // previous stream so its young sequence numbers aren't taken
            // for late packets.
            let _ = self.cmd.try_send(RxCmd::Reset);
            self.peer_state.clear();
            link.send(Message::AudioStart(self.request));
            self.requesting = true;
            self.requested_session = Some(link.session_id);
        }
    }

    fn stop_request(&mut self, link: Option<&Link>) {
        if self.requesting {
            tracing::info!(stream = self.stream.name(), "stopping audio stream");
            if let Some(l) = link {
                l.send(Message::AudioStop(AudioStop {
                    stream: self.stream,
                }));
            }
            self.requesting = false;
            self.requested_session = None;
            self.peer_state.clear();
        }
    }

    fn on_link(&mut self, link: Option<&Link>) {
        if self.requesting && link.is_some_and(|l| Some(l.session_id) == self.requested_session) {
            return; // already requested on this session (tick ran before the watch)
        }
        self.requesting = false;
        self.requested_session = None;
        self.demand_gone_since = None;
        self.peer_state.clear();
        let _ = self.cmd.try_send(RxCmd::Reset);
    }

    fn on_datagram(&mut self, d: Bytes, link: Option<&Link>) {
        let ts = proto::unwrap(&d).map_or(0, |(_, ts, _)| ts);
        // 0 = unknown: no clock-offset estimate yet (first heartbeat pong
        // arrives within ~5 s of connecting).
        let capture_local_us = match link {
            Some(l) if l.clock.rtt_us() > 0 && ts > 0 => l.clock.peer_to_local(ts).max(1),
            _ => 0,
        };
        // Full = the receiver thread is stuck; dropping is the right call.
        let _ = self.cmd.try_send(RxCmd::Packet {
            data: d,
            capture_local_us,
        });
    }

    fn on_device_failed(&mut self, why: String) {
        tracing::warn!(stream = self.stream.name(), "audio device failed: {why}");
        self.device = None;
        self.retry_at = Some(Instant::now() + Duration::from_millis(500));
    }

    fn last_packet(&self) -> Option<Instant> {
        self.shared.lock().expect("rx lock").last_packet
    }

    /// Periodic: virtual-device demand → request/stop; playback idle close;
    /// reopen failed devices.
    fn tick(&mut self, shared: &Shared, cfg: &AudioConfig) {
        let link = shared.link();
        if let Some(at) = self.retry_at {
            if Instant::now() >= at {
                let wanted = match self.kind {
                    RxDevice::Virtual => true,
                    RxDevice::PlaybackOnAudio => self
                        .last_packet()
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(cfg.idle_close_secs)),
                };
                if wanted {
                    if let Err(e) = self.open(shared) {
                        tracing::debug!(stream = self.stream.name(), "reopen failed: {e}");
                        self.retry_at = Some(Instant::now() + RETRY);
                    }
                } else {
                    self.retry_at = None;
                }
            }
        }
        match self.kind {
            RxDevice::Virtual => {
                let Some(l) = link.as_ref() else { return };
                let demand = cfg.mic_mode == AudioMode::Always
                    || self.device.as_ref().is_some_and(|d| d.active());
                if demand {
                    self.demand_gone_since = None;
                    self.request(l);
                } else if self.requesting {
                    if self.demand_gone_since.is_none() {
                        tracing::debug!(
                            stream = self.stream.name(),
                            "virtual device reports no reader"
                        );
                    }
                    let since = *self.demand_gone_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= MIC_IDLE_STOP {
                        tracing::info!("no app is recording from the virtual microphone");
                        self.stop_request(Some(l));
                    }
                }
            }
            RxDevice::PlaybackOnAudio => {
                if let Some(l) = link.as_ref() {
                    self.request(l);
                }
                let idle = self
                    .last_packet()
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(cfg.idle_close_secs));
                if idle && self.device.take().is_some() {
                    tracing::info!(
                        stream = self.stream.name(),
                        "no audio for a while; output closed"
                    );
                }
                // Audio is arriving but the output is closed (missed edge,
                // failed open): open it, unless a retry is already scheduled.
                let recent = self
                    .last_packet()
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(500));
                if recent && self.device.is_none() && self.retry_at.is_none() {
                    self.on_audio(shared);
                }
            }
        }
    }

    /// Audio started arriving: open the playback device if needed.
    fn on_audio(&mut self, shared: &Shared) {
        if self.kind == RxDevice::PlaybackOnAudio && self.device.is_none() {
            if let Err(e) = self.open(shared) {
                tracing::warn!(stream = self.stream.name(), "cannot open output: {e}");
                self.retry_at = Some(Instant::now() + RETRY);
            }
        }
    }

    /// Once a second: stats for status + feedback to the sender.
    fn report(&mut self, shared: &Shared, window: Duration) -> AudioStats {
        let (st, mut lat) = {
            let mut s = self.shared.lock().expect("rx lock");
            (s.stats, std::mem::take(&mut s.net_lat_us))
        };
        let j = self.jb.lock().expect("jitter lock").stats();
        let (dp, dseq) = (
            st.packets.saturating_sub(self.last.packets),
            st.max_seq.wrapping_sub(self.last.max_seq) as u64,
        );
        let restarted = st.restarts != self.last.restarts;
        let loss_permille = if restarted || dseq == 0 || dseq > 100_000 {
            0
        } else {
            (dseq.saturating_sub(dp) * 1000 / dseq) as u32
        };
        let dframes = st.frames.saturating_sub(self.last.frames);
        let dplc = st.plc.saturating_sub(self.last.plc);
        let plc_permille = (dplc * 1000).checked_div(dframes).unwrap_or(0) as u32;
        let dunder = j.underruns.saturating_sub(self.last_underruns);
        self.last = st;
        self.last_underruns = j.underruns;
        lat.sort_unstable();
        let net_ms = lat.get(lat.len() / 2).map(|v| *v as f64 / 1000.0);
        let latency_ms = net_ms.map_or(0.0, |n| {
            n + j.avg_ms as f64 + CODEC_DELAY_MS + self.period_ms
        });
        let receiving = dp > 0;
        if receiving && self.requesting {
            if let Some(l) = shared.link() {
                l.send(Message::AudioFeedback(AudioFeedback {
                    stream: self.stream,
                    plc_permille: plc_permille.min(1000) as u16,
                    loss_permille: loss_permille.min(1000) as u16,
                    buffer_ms: j.avg_ms.min(u16::MAX as u32) as u16,
                    target_ms: j.target_ms.min(u16::MAX as u32) as u16,
                    peak_gap_ms: j.peak_gap_ms.min(u16::MAX as u32) as u16,
                    underruns: dunder.min(u16::MAX as u64) as u16,
                    latency_ms: latency_ms.min(u16::MAX as f64) as u16,
                }));
            }
        }
        let info = self.device.as_ref().map(|d| d.info()).unwrap_or_default();
        let cb = self
            .device
            .as_ref()
            .map_or(0.0, |d| d.take_max_callback_gap_ms());
        let state = if receiving {
            if j.playing { "playing" } else { "buffering" }.to_string()
        } else if !self.peer_state.is_empty() && self.peer_state != "Streaming" {
            format!("peer {}", self.peer_state)
        } else if self.requesting {
            "waiting for audio".into()
        } else if self.device.is_some() {
            "ready (idle)".into()
        } else {
            "idle".into()
        };
        let secs = window.as_secs_f64().max(0.001);
        AudioStats {
            stream: self.stream.name().into(),
            direction: "recv".into(),
            state,
            device: info.device,
            sample_rate: info.sample_rate,
            bitrate_kbps: self.sender_rate,
            frame_ms: self.request.frame_ms_x10 as f64 / 10.0,
            redundancy: self.request.redundancy,
            buffer_ms: j.avg_ms,
            target_ms: j.target_ms,
            peak_gap_ms: j.peak_gap_ms,
            recovered: st.recovered,
            plc: st.plc,
            late: st.late,
            underruns: j.underruns,
            trims: j.trimmed,
            quiet_skips: j.quiet_skips,
            speed_pct: j.speed_pct,
            loss_permille,
            latency_ms,
            callback_gap_ms: cb,
            packets_per_s: dp as f64 / secs,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Feature task
// ---------------------------------------------------------------------------

pub async fn run(
    shared: Arc<Shared>,
    mut rx: mpsc::Receiver<Message>,
    mut dgram: mpsc::Receiver<Bytes>,
) {
    let cfg = shared.cfg.audio.clone();
    if !cfg.enabled || (!cfg.mic && !cfg.speaker) {
        shared.status.feature("audio", "disabled");
        // Still answer requests so the peer shows a clear reason.
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if let (Message::AudioStart(r), Some(l)) = (m, shared.link()) {
                    l.send(Message::AudioStatus(AudioStatus {
                        stream: r.stream,
                        state: CamState::Unavailable,
                        detail: "audio sharing is disabled in the peer's config".into(),
                        sample_rate: 0,
                        bitrate_kbps: 0,
                    }));
                }
            }
        });
        super::drain(dgram);
        return;
    }
    let source = shared.is_audio_source();
    let (mic_ev_tx, mut mic_ev) = mpsc::channel::<AudioEvent>(8);
    let (spk_ev_tx, mut spk_ev) = mpsc::channel::<AudioEvent>(8);
    let (seen_tx, mut seen_rx) = mpsc::channel::<()>(8);

    // Source (Mac): mic = send (capture on request), speaker = receive (playback on audio).
    // Sink (Ubuntu): mic = receive (virtual source), speaker = send (virtual sink).
    let mut tx: Option<TxStream>;
    let mut rxs: Option<RxStream> = None;
    if source {
        tx = Some(TxStream::new(
            AudioStream::Mic,
            TxDevice::CaptureOnRequest,
            mic_ev_tx,
        ));
        if cfg.speaker {
            rxs = Some(RxStream::new(
                AudioStream::Speaker,
                RxDevice::PlaybackOnAudio,
                &cfg,
                spk_ev_tx,
                seen_tx,
            ));
        }
    } else {
        if cfg.mic {
            let mut r = RxStream::new(
                AudioStream::Mic,
                RxDevice::Virtual,
                &cfg,
                mic_ev_tx,
                seen_tx,
            );
            if let Err(e) = r.open(&shared) {
                tracing::warn!("virtual microphone unavailable: {e}");
                r.retry_at = Some(Instant::now() + RETRY);
            }
            rxs = Some(r);
        }
        let mut t = TxStream::new(AudioStream::Speaker, TxDevice::Virtual, spk_ev_tx);
        if cfg.speaker {
            if let Err(e) = t.open(&shared) {
                tracing::warn!("virtual speaker unavailable: {e}");
                t.state = format!("unavailable ({e})");
                t.retry_at = Some(Instant::now() + RETRY);
            }
        }
        tx = Some(t);
    }
    let tx_enabled = if source { cfg.mic } else { cfg.speaker };
    let health = |tx: &Option<TxStream>, rx: &Option<RxStream>| -> String {
        let mut parts = Vec::new();
        if let Some(r) = rx {
            parts.push(match (&r.device, r.kind) {
                (Some(d), RxDevice::Virtual) => format!("{} ready", d.info().device),
                (None, RxDevice::Virtual) => format!("{} unavailable", r.stream.name()),
                (_, RxDevice::PlaybackOnAudio) => "speaker → output on demand".to_string(),
            });
        }
        if let Some(t) = tx {
            if tx_enabled {
                parts.push(match (&t.device, t.kind) {
                    (Some(d), TxDevice::Virtual) => format!("{} ready", d.info().device),
                    (None, TxDevice::Virtual) => format!("{} {}", t.stream.name(), t.state),
                    (_, TxDevice::CaptureOnRequest) => format!("mic {}", t.state),
                });
            }
        }
        format!(
            "{}: {}",
            if source { "source" } else { "sink" },
            parts.join(", ")
        )
    };
    shared.status.feature("audio", health(&tx, &rxs));

    let mut link_rx = shared.watch_link();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut report = tokio::time::interval(Duration::from_secs(1));
    report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_report = Instant::now();
    let rx_tag = if source { TAG_SPEAKER } else { TAG_MIC };

    loop {
        tokio::select! {
            changed = link_rx.changed() => {
                if changed.is_err() { return; }
                let link = link_rx.borrow_and_update().clone();
                if let Some(t) = tx.as_mut() { t.on_link(link.clone()); }
                if let Some(r) = rxs.as_mut() {
                    r.on_link(link.as_ref());
                    r.tick(&shared, &cfg); // request immediately on a new link
                }
            }
            Some(msg) = rx.recv() => match msg {
                Message::AudioStart(req) => match tx.as_mut() {
                    Some(t) if t.stream == req.stream => t.on_start(&shared, req, tx_enabled),
                    _ => if let Some(l) = shared.link() {
                        l.send(Message::AudioStatus(AudioStatus {
                            stream: req.stream,
                            state: CamState::Unavailable,
                            detail: format!("{} cannot send the {} stream (role mismatch?)", shared.identity.device_name, req.stream.name()),
                            sample_rate: 0,
                        bitrate_kbps: 0,
                        }));
                    },
                },
                Message::AudioStop(s) => if let Some(t) = tx.as_mut() {
                    if t.stream == s.stream { t.on_stop(); }
                },
                Message::AudioFeedback(fb) => if let Some(t) = tx.as_mut() {
                    if t.stream == fb.stream { t.on_feedback(&shared, fb); }
                },
                Message::AudioStatus(s) => if let Some(r) = rxs.as_mut() {
                    if r.stream == s.stream {
                        tracing::info!(stream = s.stream.name(), state = ?s.state, detail = %s.detail, "peer audio state");
                        if s.state == CamState::Streaming {
                            r.sender_rate = s.bitrate_kbps;
                        }
                        r.peer_state = if s.detail.is_empty() { format!("{:?}", s.state) } else { format!("{:?} ({})", s.state, s.detail) };
                    }
                },
                _ => {}
            },
            Some(d) = dgram.recv() => {
                if let Some(r) = rxs.as_mut() {
                    if d.first() == Some(&rx_tag) {
                        let link = shared.link();
                        r.on_datagram(d, link.as_ref());
                    }
                }
            }
            Some(()) = seen_rx.recv() => {
                if let Some(r) = rxs.as_mut() { r.on_audio(&shared); }
            }
            Some(ev) = mic_ev.recv() => {
                let AudioEvent::Failed(why) = ev;
                if source { if let Some(t) = tx.as_mut() { t.on_device_failed(&shared, why); } }
                else if let Some(r) = rxs.as_mut() { r.on_device_failed(why); }
            }
            Some(ev) = spk_ev.recv() => {
                let AudioEvent::Failed(why) = ev;
                if source { if let Some(r) = rxs.as_mut() { r.on_device_failed(why); } }
                else if let Some(t) = tx.as_mut() { t.on_device_failed(&shared, why); }
            }
            _ = tick.tick() => {
                if let Some(r) = rxs.as_mut() { r.tick(&shared, &cfg); }
                if let Some(t) = tx.as_mut() { t.tick(&shared); }
            }
            _ = report.tick() => {
                let window = last_report.elapsed();
                last_report = Instant::now();
                if let Some(r) = rxs.as_mut() {
                    let st = r.report(&shared, window);
                    shared.status.audio(st);
                }
                if let Some(t) = tx.as_mut() {
                    if tx_enabled {
                        let st = t.stats(window);
                        shared.status.audio(st);
                    }
                }
                shared.status.feature("audio", health(&tx, &rxs));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips_into_sender_params() {
        let cfg = AudioConfig {
            frame_ms: 2.5,
            redundancy: 3,
            bitrate_kbps: 192,
            ..Default::default()
        };
        let req = start_request(&cfg, AudioStream::Mic);
        assert_eq!(req.frame_ms_x10, 25);
        let (p, rate) = sender_params(&req, 1200);
        assert_eq!(p.frame_ms, 2.5);
        assert_eq!(p.redundancy, 3);
        assert_eq!(p.bitrate_bps, 192_000);
        assert_eq!(p.max_datagram, 1200);
        assert!(!rate.is_auto());
    }

    #[test]
    fn hostile_request_is_clamped() {
        let req = AudioStart {
            stream: AudioStream::Speaker,
            bitrate_kbps: 0,
            min_bitrate_kbps: 0,
            max_bitrate_kbps: 99_999,
            frame_ms_x10: 7,
            redundancy: 200,
            complexity: 99,
        };
        let (p, rate) = sender_params(&req, 1200);
        assert_eq!(p.frame_ms, 5.0);
        assert_eq!(p.redundancy, 3);
        assert_eq!(p.complexity, 10);
        assert!(rate.is_auto());
        assert!((6_000..=510_000).contains(&p.bitrate_bps));
    }

    #[test]
    fn tags_differ_per_stream() {
        assert_ne!(tag_of(AudioStream::Mic), tag_of(AudioStream::Speaker));
    }
}
