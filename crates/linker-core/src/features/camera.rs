//! Camera sharing: the Mac's camera as a webcam on Ubuntu. **Video only — no
//! audio path exists anywhere in Linker.**
//!
//! * **Source** (macOS): on `CamStart`, the backend captures + hardware
//!   encodes H.264; frames are fragmented into QUIC datagrams.
//! * **Sink** (Ubuntu): datagrams are reassembled; a frame with any lost
//!   fragment is dropped and a keyframe is requested; frames go through a
//!   small jitter buffer to the backend, which decodes and writes them to the
//!   v4l2loopback device. With no stream, a placeholder frame keeps the
//!   device alive so apps never freeze.
//! * **Adaptation**: the sink reports loss/latency once a second; the source
//!   lowers the bitrate on loss and slowly raises it back when clean.
//! * **On demand** (default): the Mac camera only runs while some app on
//!   Ubuntu has the virtual device open.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use linker_platform::{now_us, CamState, CameraEvent, CameraParams, CameraSink, CameraSource};
use linker_proto::media::{fragment, Reassembler};
use linker_proto::{CamFeedback, CamStart, CamStatus, CamStop, Feature, Message};
use tokio::sync::mpsc;

use crate::config::CameraMode;
use crate::status::CameraStats;
use crate::Shared;

pub async fn run(shared: Arc<Shared>, rx: mpsc::Receiver<Message>, dgram: mpsc::Receiver<Bytes>) {
    if !shared.cfg.camera.enabled {
        shared.status.feature("camera", "disabled");
        super::drain(rx);
        super::drain(dgram);
        return;
    }
    if shared.is_camera_source() {
        super::drain(dgram);
        run_source(shared, rx).await
    } else {
        run_sink(shared, rx, dgram).await
    }
}

// ---------------------------------------------------------------------------
// Source (Mac)
// ---------------------------------------------------------------------------

/// Bitrate controller (unit-tested).
#[derive(Debug)]
pub struct RateControl {
    pub kbps: u32,
    min: u32,
    max: u32,
    clean_reports: u32,
}

impl RateControl {
    pub fn new(start: u32, min: u32, max: u32) -> Self {
        RateControl {
            kbps: start.clamp(min, max),
            min,
            max,
            clean_reports: 0,
        }
    }

    /// Returns the new bitrate if it changed.
    pub fn on_feedback(&mut self, loss_permille: u16) -> Option<u32> {
        let old = self.kbps;
        if loss_permille > 20 {
            self.kbps = ((self.kbps as f64 * 0.75) as u32).max(self.min);
            self.clean_reports = 0;
        } else if loss_permille == 0 {
            self.clean_reports += 1;
            if self.clean_reports >= 5 {
                self.kbps = ((self.kbps as f64 * 1.1) as u32).min(self.max);
                self.clean_reports = 0;
            }
        } else {
            self.clean_reports = 0;
        }
        (self.kbps != old).then_some(self.kbps)
    }
}

async fn run_source(shared: Arc<Shared>, mut rx: mpsc::Receiver<Message>) {
    let cfg = shared.cfg.camera.clone();
    shared.status.feature("camera", "source: idle");
    let (ev_tx, mut ev_rx) = mpsc::channel::<CameraEvent>(8);
    let mut source: Option<Box<dyn CameraSource>> = None;
    let mut rate = RateControl::new(cfg.bitrate_kbps, cfg.min_bitrate_kbps, cfg.max_bitrate_kbps);
    let mut frame_id = 0u32;
    let mut link_rx = shared.watch_link();
    let mut sent = 0u32;
    let mut window = Instant::now();

    loop {
        tokio::select! {
            changed = link_rx.changed() => {
                if changed.is_err() { return; }
                link_rx.borrow_and_update();
                if source.take().is_some() {
                    tracing::info!("link changed; camera stopped");
                    shared.status.feature("camera", "source: idle");
                }
            }
            Some(msg) = rx.recv() => match msg {
                Message::CamStart(CamStart { width, height, fps, bitrate_kbps }) => {
                    source = None; // release any previous session first
                    rate = RateControl::new(bitrate_kbps.min(cfg.max_bitrate_kbps), cfg.min_bitrate_kbps, cfg.max_bitrate_kbps);
                    let params = CameraParams { width, height, fps, bitrate_kbps: rate.kbps };
                    tracing::info!(?params, "camera start requested");
                    // May block (permission prompt, AVCaptureSession start).
                    let started = tokio::task::block_in_place(|| shared.backend.camera_source(params, ev_tx.clone()));
                    match started {
                        Ok(s) => {
                            source = Some(s);
                            shared.status.feature("camera", format!("source: streaming {width}x{height}@{fps}"));
                        }
                        Err(e) => {
                            tracing::warn!("camera unavailable: {}", e.0);
                            shared.status.feature("camera", format!("source: unavailable ({})", e.0));
                            if let Some(l) = shared.link() {
                                l.send(Message::CamStatus(CamStatus { state: CamState::Unavailable, detail: e.0 }));
                            }
                        }
                    }
                }
                Message::CamStop(_) => {
                    if source.take().is_some() {
                        tracing::info!("camera stop requested");
                    }
                    shared.status.feature("camera", "source: idle");
                }
                Message::CamFeedback(fb) => {
                    if let Some(s) = &source {
                        if fb.request_keyframe {
                            s.request_keyframe();
                        }
                        if let Some(kbps) = rate.on_feedback(fb.loss_permille) {
                            tracing::debug!(kbps, loss = fb.loss_permille, "camera bitrate adjusted");
                            s.set_bitrate(kbps);
                        }
                    }
                    shared.status.update(|st| st.camera = Some(CameraStats {
                        state: "streaming".into(),
                        fps: fb.fps_received as f64,
                        latency_ms: fb.latency_ms as f64,
                        loss_permille: fb.loss_permille as u32,
                        bitrate_kbps: rate.kbps,
                        resolution: format!("{}x{}", cfg.width, cfg.height),
                        decoder: "peer".into(),
                        device: "local camera".into(),
                    }));
                }
                _ => {}
            },
            Some(ev) = ev_rx.recv() => {
                let Some(link) = shared.link() else { continue };
                match ev {
                    CameraEvent::Frame(f) => {
                        if source.is_none() { continue; }
                        let Some(max) = link.max_datagram() else {
                            shared.status.feature("camera", "source: peer does not accept datagrams");
                            continue;
                        };
                        frame_id = frame_id.wrapping_add(1);
                        for d in fragment(&f.data, frame_id, f.keyframe, f.capture_ts_us, max.min(1400)) {
                            link.send_datagram(Bytes::from(d));
                        }
                        sent += 1;
                        if window.elapsed() >= Duration::from_secs(5) {
                            tracing::debug!(fps = sent as f64 / window.elapsed().as_secs_f64(), "camera sending");
                            sent = 0;
                            window = Instant::now();
                        }
                    }
                    CameraEvent::State { state, detail } => {
                        tracing::info!(?state, %detail, "camera state");
                        if state != CamState::Streaming {
                            shared.status.feature("camera", format!("source: {state:?} ({detail})"));
                        }
                        link.send(Message::CamStatus(CamStatus { state, detail }));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sink (Ubuntu)
// ---------------------------------------------------------------------------

enum SinkCmd {
    Frame {
        data: Vec<u8>,
        capture_local_us: u64,
    },
    /// Stream stopped / unavailable: show placeholder.
    Idle,
    Reset,
}

struct SinkReport {
    consumers: usize,
    /// Latencies (µs, capture → written to device) since the last report.
    latencies: Vec<u64>,
    written: u32,
}

/// Small playout buffer: frames are released spaced like their capture
/// timestamps, `jitter` after arrival of the first frame of a run; if the
/// schedule drifts too far it resets (never lets latency accumulate).
struct Jitter {
    base: Option<(Instant, u64)>,
    jitter: Duration,
}

impl Jitter {
    fn due(&mut self, capture_us: u64) -> Instant {
        let now = Instant::now();
        if let Some((t0, c0)) = self.base {
            if capture_us >= c0 {
                let due = t0 + Duration::from_micros(capture_us - c0) + self.jitter;
                // Too late (network delay grew) or way early (clock reset): rebase.
                if due + Duration::from_millis(60) >= now && due <= now + Duration::from_millis(200)
                {
                    return due.max(now);
                }
            }
        }
        self.base = Some((now, capture_us));
        now + self.jitter
    }
}

fn sink_thread(
    mut sink: Box<dyn CameraSink>,
    cmd_rx: std::sync::mpsc::Receiver<SinkCmd>,
    report: mpsc::Sender<SinkReport>,
    jitter_ms: u32,
    fps: u32,
) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut jitter = Jitter {
        base: None,
        jitter: Duration::from_millis(jitter_ms as u64),
    };
    let mut last_frame = Instant::now() - Duration::from_secs(10);
    let mut last_report = Instant::now();
    let mut latencies = Vec::new();
    let mut written = 0u32;
    let placeholder_every = Duration::from_millis(200);
    let mut last_placeholder = Instant::now() - placeholder_every;
    let mut pending: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
    let frame_interval = Duration::from_micros(1_000_000 / fps.max(1) as u64);

    loop {
        let timeout = if pending.is_empty() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(1)
        };
        match cmd_rx.recv_timeout(timeout) {
            Ok(SinkCmd::Frame {
                data,
                capture_local_us,
            }) => pending.push_back((data, capture_local_us)),
            Ok(SinkCmd::Reset) => {
                pending.clear();
                sink.reset_decoder();
                jitter.base = None;
            }
            Ok(SinkCmd::Idle) => {
                pending.clear();
                last_frame = Instant::now() - Duration::from_secs(10);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        // Never let the buffer grow: if it does, drop to the newest few frames.
        // Every frame must still be decoded (P-frames depend on predecessors),
        // so we decode-and-skip rather than discard bitstream.
        while let Some((_, cap)) = pending.front() {
            let due = jitter.due(*cap);
            if due > Instant::now() && pending.len() < 4 {
                break;
            }
            let (data, cap) = pending.pop_front().expect("non-empty");
            match sink.write_encoded(&data) {
                Ok(true) => {
                    written += 1;
                    latencies.push(now_us().saturating_sub(cap));
                    last_frame = Instant::now();
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::debug!("decode/write failed: {e:#}");
                    sink.reset_decoder();
                }
            }
        }

        // Keep the device alive with a placeholder when no stream is flowing.
        if last_frame.elapsed() > frame_interval * 15
            && last_placeholder.elapsed() >= placeholder_every
        {
            if let Err(e) = sink.write_placeholder() {
                tracing::debug!("placeholder write failed: {e:#}");
            }
            last_placeholder = Instant::now();
        }

        if last_report.elapsed() >= Duration::from_secs(1) {
            let r = SinkReport {
                consumers: sink.consumers(),
                latencies: std::mem::take(&mut latencies),
                written,
            };
            written = 0;
            last_report = Instant::now();
            if report.blocking_send(r).is_err() {
                return;
            }
        }
    }
}

async fn run_sink(
    shared: Arc<Shared>,
    mut rx: mpsc::Receiver<Message>,
    mut dgram: mpsc::Receiver<Bytes>,
) {
    let cfg = shared.cfg.camera.clone();
    let params = CameraParams {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        bitrate_kbps: cfg.bitrate_kbps,
    };
    let sink = match shared.backend.camera_sink(params) {
        Ok(s) => s,
        Err(e) => {
            shared
                .status
                .feature("camera", format!("unsupported ({})", e.0));
            super::drain(rx);
            super::drain(dgram);
            return;
        }
    };
    let device = sink.device_path();
    let decoder = sink.decoder_name();
    shared.status.feature(
        "camera",
        format!("sink: {device} ready (waiting for an app / the Mac)"),
    );

    let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel::<SinkCmd>(16);
    let (rep_tx, mut rep_rx) = mpsc::channel::<SinkReport>(4);
    let (jitter_ms, fps) = (cfg.jitter_ms, cfg.fps);
    std::thread::Builder::new()
        .name("linker-camera-sink".into())
        .spawn(move || sink_thread(sink, cmd_rx, rep_tx, jitter_ms, fps))
        .expect("spawn sink thread");

    let mut reasm = Reassembler::new();
    let mut streaming = false;
    let mut need_key = true;
    let mut last_key_req = Instant::now() - Duration::from_secs(1);
    let mut consumers = 0usize;
    let mut no_consumer_since: Option<Instant> = None;
    let mut received = 0u32;
    let mut lost_window = 0u32;
    let mut peer_state = String::from("idle");
    let mut link_rx = shared.watch_link();
    let mut tick = tokio::time::interval(Duration::from_millis(500));

    let start_msg = Message::CamStart(CamStart {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        bitrate_kbps: cfg.bitrate_kbps,
    });

    loop {
        tokio::select! {
            changed = link_rx.changed() => {
                if changed.is_err() { return; }
                link_rx.borrow_and_update();
                // New or lost link: stream state is gone either way.
                streaming = false;
                reasm.reset();
                need_key = true;
                let _ = cmd_tx.try_send(SinkCmd::Idle);
            }

            Some(r) = rep_rx.recv() => {
                consumers = r.consumers;
                let mut lat = r.latencies;
                lat.sort_unstable();
                let median_ms = lat.get(lat.len() / 2).map(|v| *v as f64 / 1000.0).unwrap_or(0.0);
                let lost = std::mem::take(&mut lost_window);
                let total = received + lost;
                let loss_permille = (lost * 1000).checked_div(total).unwrap_or(0) as u16;
                received = 0;
                if streaming {
                    if let Some(l) = shared.link() {
                        l.send(Message::CamFeedback(CamFeedback {
                            loss_permille,
                            latency_ms: median_ms as u32,
                            fps_received: r.written as u16,
                            request_keyframe: need_key,
                        }));
                    }
                }
                shared.status.update(|s| s.camera = Some(CameraStats {
                    state: if streaming { format!("streaming ({peer_state})") } else { "idle (placeholder)".into() },
                    fps: r.written as f64,
                    latency_ms: median_ms,
                    loss_permille: loss_permille as u32,
                    bitrate_kbps: cfg.bitrate_kbps,
                    resolution: format!("{}x{}", cfg.width, cfg.height),
                    decoder: decoder.clone(),
                    device: format!("{device} ({consumers} app(s) reading)"),
                }));
                if streaming && r.written > 0 {
                    tracing::trace!(fps = r.written, latency_ms = median_ms, loss_permille, "camera sink");
                }
            }

            _ = tick.tick() => {
                let Some(link) = shared.link() else { continue };
                if !link.peer_has(Feature::CameraSource) { continue; }
                let demand = cfg.mode == CameraMode::Always || consumers > 0;
                if demand {
                    no_consumer_since = None;
                    if !streaming {
                        tracing::info!(consumers, "requesting camera stream from peer");
                        link.send(start_msg.clone());
                        streaming = true;
                        need_key = true;
                        reasm.reset();
                        shared.status.feature("camera", format!("sink: {device} streaming from {}", link.peer.device_name));
                    }
                } else if streaming {
                    let since = *no_consumer_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > Duration::from_secs(5) {
                        tracing::info!("no app is using the camera; stopping stream");
                        link.send(Message::CamStop(CamStop));
                        streaming = false;
                        let _ = cmd_tx.try_send(SinkCmd::Idle);
                        shared.status.feature("camera", format!("sink: {device} ready (idle)"));
                    }
                }
            }

            Some(d) = dgram.recv() => {
                if !streaming { continue; }
                let Some(frame) = reasm.push(&d) else { continue };
                let lost = reasm.take_lost();
                if lost > 0 {
                    lost_window += lost;
                    need_key = true;
                }
                received += 1;
                if need_key && !frame.keyframe {
                    // Can't decode a P-frame after a gap; ask for a keyframe (rate-limited).
                    if last_key_req.elapsed() > Duration::from_millis(300) {
                        last_key_req = Instant::now();
                        if let Some(l) = shared.link() {
                            l.send(Message::CamFeedback(CamFeedback {
                                loss_permille: 1000, latency_ms: 0, fps_received: 0, request_keyframe: true,
                            }));
                        }
                    }
                    continue;
                }
                if frame.keyframe {
                    if need_key {
                        let _ = cmd_tx.try_send(SinkCmd::Reset);
                    }
                    need_key = false;
                }
                let capture_local_us = shared.link().map_or(frame.capture_ts_us, |l| l.clock.peer_to_local(frame.capture_ts_us));
                if cmd_tx.try_send(SinkCmd::Frame { data: frame.data, capture_local_us }).is_err() {
                    // Decoder can't keep up: drop and resync at the next keyframe.
                    need_key = true;
                    lost_window += 1;
                }
            }

            Some(msg) = rx.recv() => {
                if let Message::CamStatus(CamStatus { state, detail }) = msg {
                    tracing::info!(?state, %detail, "peer camera state");
                    peer_state = format!("{state:?}");
                    if matches!(state, CamState::Busy | CamState::Unavailable | CamState::Stopped) {
                        let _ = cmd_tx.try_send(SinkCmd::Idle);
                        need_key = true;
                        shared.status.feature("camera", format!("sink: {device} — Mac camera {state:?}: {detail}"));
                    }
                    if state == CamState::Unavailable {
                        // Don't hammer a camera that's denied; retry on next demand edge.
                        streaming = true;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_drops_on_loss_and_recovers_slowly() {
        let mut r = RateControl::new(4000, 1000, 6000);
        assert_eq!(r.on_feedback(100), Some(3000));
        assert_eq!(r.on_feedback(100), Some(2250));
        for _ in 0..4 {
            assert_eq!(r.on_feedback(0), None);
        }
        assert_eq!(r.on_feedback(0), Some(2475));
        // Never below min.
        for _ in 0..20 {
            r.on_feedback(500);
        }
        assert_eq!(r.kbps, 1000);
    }

    #[test]
    fn jitter_schedules_by_capture_time() {
        let mut j = Jitter {
            base: None,
            jitter: Duration::from_millis(20),
        };
        let t0 = j.due(1_000_000);
        let t1 = j.due(1_033_333);
        let d = t1.duration_since(t0);
        assert!(
            d >= Duration::from_millis(30) && d <= Duration::from_millis(40),
            "{d:?}"
        );
    }
}
