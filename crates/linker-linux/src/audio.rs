//! Audio on Ubuntu through PipeWire (userspace, no kernel module).
//!
//! Every Linker audio device is one `pw_stream` owned by `linkerd`, running
//! its process callback on PipeWire's real-time data thread (`RT_PROCESS`):
//!
//! | API              | PipeWire node                                                          |
//! |------------------|------------------------------------------------------------------------|
//! | `virtual_mic`    | OUTPUT stream, `media.class = Audio/Source`, `node.name = linker_microphone` — apps record from it |
//! | `virtual_speaker`| INPUT stream, `media.class = Audio/Sink`, `node.name = linker_speaker` — apps play to it |
//! | `audio_capture`  | INPUT stream, autoconnected (optionally to `target.object`)            |
//! | `audio_playback` | OUTPUT stream, autoconnected (optionally to `target.object`)           |
//!
//! The virtual nodes are not autoconnected: the session manager (WirePlumber)
//! offers them to applications like any device — PulseAudio clients (Zoom,
//! Chrome, Firefox) see them through `pipewire-pulse` as a source "Linker
//! Microphone" and a sink "Linker Speaker". They exist exactly as long as
//! `linkerd` keeps the stream, so a crash never leaves stale devices.
//!
//! A virtual sink node receives exactly the audio applications route to it
//! — the same as capturing a null-sink's monitor, without the extra hop.
//!
//! All streams negotiate S16LE / 48 kHz / stereo; PipeWire's adapter
//! converts to and from whatever the rest of the graph uses.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use linker_platform::{
    AudioEvent, AudioSpec, AudioStream, AudioStreamInfo, Cap, PcmSink, PcmSource, Unsupported,
    AUDIO_CHANNELS, AUDIO_RATE,
};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use tokio::sync::mpsc;

const STRIDE: usize = 2 * AUDIO_CHANNELS; // S16 stereo
/// A virtual device counts as "in use" while its node ran this recently.
const ACTIVE_WINDOW: Duration = Duration::from_millis(300);
const START_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    VirtualMic,
    VirtualSpeaker,
    Capture,
    Playback,
}

impl Kind {
    fn output(self) -> bool {
        matches!(self, Kind::VirtualMic | Kind::Playback)
    }
}

enum Io {
    Source(PcmSource),
    Sink(PcmSink),
}

/// Shared between the PipeWire thread and the handle.
#[derive(Default)]
struct Meters {
    /// ns since `epoch` of the last process callback (0 = never).
    last_cb_ns: AtomicU64,
    max_gap_ns: AtomicU64,
    period_frames: AtomicU32,
    streaming: AtomicBool,
}

pub struct PwStream {
    quit: Option<pw::channel::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
    meters: Arc<Meters>,
    epoch: Instant,
    kind: Kind,
    device: String,
}

impl AudioStream for PwStream {
    fn info(&self) -> AudioStreamInfo {
        AudioStreamInfo {
            device: self.device.clone(),
            sample_rate: AUDIO_RATE,
            channels: AUDIO_CHANNELS as u16,
            period_frames: self.meters.period_frames.load(Ordering::Relaxed),
            device_latency_ms: 0.0,
        }
    }

    fn active(&self) -> bool {
        match self.kind {
            Kind::VirtualMic | Kind::VirtualSpeaker => {
                // Only the process callback is trusted: PipeWire schedules
                // the node only while something is linked to it. The stream
                // *state* is not reliable for this — on a graph reconfiguration
                // it was observed to stay "Paused" for 15 s while process()
                // kept running and an app kept recording (runbook/measurements.md).
                let last = self.meters.last_cb_ns.load(Ordering::Relaxed);
                last != 0
                    && (self.epoch.elapsed().as_nanos() as u64).saturating_sub(last)
                        < ACTIVE_WINDOW.as_nanos() as u64
            }
            _ => true,
        }
    }

    fn take_max_callback_gap_ms(&self) -> f64 {
        self.meters.max_gap_ns.swap(0, Ordering::Relaxed) as f64 / 1e6
    }
}

impl Drop for PwStream {
    fn drop(&mut self) {
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(pw::init);
}

pub fn virtual_mic(
    spec: AudioSpec,
    src: PcmSource,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    open(Kind::VirtualMic, spec, Io::Source(src), ev)
}

pub fn virtual_speaker(
    spec: AudioSpec,
    sink: PcmSink,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    open(Kind::VirtualSpeaker, spec, Io::Sink(sink), ev)
}

pub fn capture(
    spec: AudioSpec,
    sink: PcmSink,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    open(Kind::Capture, spec, Io::Sink(sink), ev)
}

pub fn playback(
    spec: AudioSpec,
    src: PcmSource,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    open(Kind::Playback, spec, Io::Source(src), ev)
}

fn open(
    kind: Kind,
    spec: AudioSpec,
    io: Io,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    init();
    let device = match kind {
        Kind::VirtualMic | Kind::VirtualSpeaker => {
            if spec.description.is_empty() {
                spec.device.clone().unwrap_or_default()
            } else {
                spec.description.clone()
            }
        }
        _ => spec.device.clone().unwrap_or_else(|| "default".into()),
    };
    let meters = Arc::new(Meters::default());
    let epoch = Instant::now();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let m = meters.clone();
    let join = std::thread::Builder::new()
        .name(format!("linker-pw-{kind:?}").to_lowercase())
        .spawn(move || {
            if let Err(e) = run(kind, spec, io, ev, m, epoch, quit_rx, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| Unsupported::new(format!("spawn: {e}")))?;
    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(Box::new(PwStream {
            quit: Some(quit_tx),
            join: Some(join),
            meters,
            epoch,
            kind,
            device,
        })),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(Unsupported::new(e))
        }
        Err(_) => {
            let _ = quit_tx.send(());
            Err(Unsupported::new(
                "PipeWire did not answer within 5 s (is the user's PipeWire running? `systemctl --user status pipewire`)",
            ))
        }
    }
}

fn props_for(kind: Kind, spec: &AudioSpec) -> pw::properties::PropertiesBox {
    let quantum = ((AUDIO_RATE as f64 * spec.period_ms / 1000.0).round() as u32).clamp(32, 8192);
    let latency = format!("{quantum}/{AUDIO_RATE}");
    let mut p = properties! {
        "media.type" => "Audio",
        "audio.channels" => "2",
        "audio.position" => "[ FL FR ]",
        "audio.rate" => AUDIO_RATE.to_string(),
        "node.latency" => latency,
        "application.name" => "Linker",
        "application.id" => "io.linker.daemon",
    };
    match kind {
        Kind::VirtualMic => {
            p.insert("media.class", "Audio/Source");
            p.insert(
                "node.name",
                spec.device
                    .clone()
                    .unwrap_or_else(|| "linker_microphone".into()),
            );
            p.insert("node.description", spec.description.clone());
            p.insert("node.nick", spec.description.clone());
            p.insert("device.icon-name", "audio-input-microphone");
            p.insert("media.role", "Communication");
            // Never let the session manager link our source to a speaker.
            p.insert("node.autoconnect", "false");
        }
        Kind::VirtualSpeaker => {
            p.insert("media.class", "Audio/Sink");
            p.insert(
                "node.name",
                spec.device
                    .clone()
                    .unwrap_or_else(|| "linker_speaker".into()),
            );
            p.insert("node.description", spec.description.clone());
            p.insert("node.nick", spec.description.clone());
            p.insert("device.icon-name", "audio-speakers");
            p.insert("media.role", "Communication");
            p.insert("node.autoconnect", "false");
        }
        Kind::Capture => {
            p.insert("media.category", "Capture");
            p.insert("media.role", "Communication");
            p.insert("node.name", "linker_capture");
            if let Some(t) = &spec.device {
                // PulseAudio convention: "<sink>.monitor" records what plays on <sink>.
                let (t, monitor) = match t.strip_suffix(".monitor") {
                    Some(sink) => (sink.to_string(), true),
                    None => (t.clone(), false),
                };
                if monitor {
                    p.insert("stream.capture.sink", "true");
                }
                p.insert("target.object", t.clone());
                p.insert("node.target", t); // PipeWire < 0.3.44
            }
        }
        Kind::Playback => {
            p.insert("media.category", "Playback");
            p.insert("media.role", "Communication");
            p.insert("node.name", "linker_playback");
            if let Some(t) = &spec.device {
                p.insert("target.object", t.clone());
                p.insert("node.target", t.clone());
            }
        }
    }
    p
}

fn format_pod() -> Result<Vec<u8>, String> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(AUDIO_RATE);
    info.set_channels(AUDIO_CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|e| format!("format pod: {e:?}"))?
    .0
    .into_inner())
}

struct UserData {
    io: Io,
    scratch: Vec<i16>,
    meters: Arc<Meters>,
    epoch: Instant,
}

/// Real-time process callback body (shared by all four kinds).
fn process(stream: &pw::stream::Stream, ud: &mut UserData, output: bool) {
    let now_ns = ud.epoch.elapsed().as_nanos() as u64;
    let last = ud.meters.last_cb_ns.swap(now_ns.max(1), Ordering::Relaxed);
    if last != 0 {
        ud.meters
            .max_gap_ns
            .fetch_max(now_ns.saturating_sub(last), Ordering::Relaxed);
    }
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = buffer.requested() as usize;
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    if output {
        let n_frames = {
            let Some(slice) = data.data() else { return };
            let cap = slice.len() / STRIDE;
            let n = if requested > 0 {
                requested.min(cap)
            } else {
                cap
            };
            if ud.scratch.len() < n * AUDIO_CHANNELS {
                ud.scratch.resize(n * AUDIO_CHANNELS, 0); // once, at the largest quantum
            }
            let pcm = &mut ud.scratch[..n * AUDIO_CHANNELS];
            match &mut ud.io {
                Io::Source(src) => src(pcm),
                Io::Sink(_) => pcm.fill(0),
            }
            let (dst, _) = slice[..n * STRIDE].as_chunks_mut::<2>();
            for (d, s) in dst.iter_mut().zip(pcm.iter()) {
                *d = s.to_le_bytes();
            }
            n
        };
        ud.meters
            .period_frames
            .store(n_frames as u32, Ordering::Relaxed);
        let chunk = data.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = STRIDE as i32;
        *chunk.size_mut() = (n_frames * STRIDE) as u32;
    } else {
        let (offset, size) = {
            let c = data.chunk();
            (c.offset() as usize, c.size() as usize)
        };
        let Some(slice) = data.data() else { return };
        let end = (offset + size).min(slice.len());
        let bytes = &slice[offset.min(end)..end];
        let n = bytes.len() / STRIDE;
        if n == 0 {
            return;
        }
        if ud.scratch.len() < n * AUDIO_CHANNELS {
            ud.scratch.resize(n * AUDIO_CHANNELS, 0);
        }
        let pcm = &mut ud.scratch[..n * AUDIO_CHANNELS];
        let (src, _) = bytes.as_chunks::<2>();
        for (d, b) in pcm.iter_mut().zip(src) {
            *d = i16::from_le_bytes(*b);
        }
        ud.meters.period_frames.store(n as u32, Ordering::Relaxed);
        if let Io::Sink(sink) = &mut ud.io {
            sink(pcm);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    kind: Kind,
    spec: AudioSpec,
    io: Io,
    ev: mpsc::Sender<AudioEvent>,
    meters: Arc<Meters>,
    epoch: Instant,
    quit_rx: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| format!("PipeWire context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| {
        format!("cannot connect to PipeWire ({e}) — is PipeWire running in this user session?")
    })?;

    // Fatal core errors (PipeWire restarted / connection lost) end the stream.
    let failed = Arc::new(AtomicBool::new(false));
    let _core_listener = {
        let ev = ev.clone();
        let ml = mainloop.clone();
        let failed = failed.clone();
        core.add_listener_local()
            .error(move |id, _seq, res, msg| {
                if id == pw::core::PW_ID_CORE {
                    if !failed.swap(true, Ordering::SeqCst) {
                        let _ = ev.try_send(AudioEvent::Failed(format!("PipeWire: {msg} ({res})")));
                    }
                    ml.quit();
                }
            })
            .register()
    };

    let name = match kind {
        Kind::VirtualMic => "Linker Microphone",
        Kind::VirtualSpeaker => "Linker Speaker",
        Kind::Capture => "Linker capture",
        Kind::Playback => "Linker playback",
    };
    let stream = pw::stream::StreamBox::new(&core, name, props_for(kind, &spec))
        .map_err(|e| format!("PipeWire stream: {e}"))?;

    let output = kind.output();
    let ud = UserData {
        io,
        scratch: Vec::with_capacity(8192 * AUDIO_CHANNELS),
        meters: meters.clone(),
        epoch,
    };
    let _listener = {
        let ev = ev.clone();
        let ml = mainloop.clone();
        let failed = failed.clone();
        let m = meters.clone();
        stream
            .add_local_listener_with_user_data(ud)
            .state_changed(move |_, _, old, new| {
                use pw::stream::StreamState as S;
                tracing::debug!(?kind, ?old, ?new, "PipeWire stream state");
                m.streaming
                    .store(matches!(new, S::Streaming), Ordering::Relaxed);
                let why = match (&old, &new) {
                    (_, S::Error(e)) => Some(format!("stream error: {e}")),
                    (S::Paused | S::Streaming, S::Unconnected) => {
                        Some("stream disconnected".to_string())
                    }
                    _ => None,
                };
                if let Some(why) = why {
                    if !failed.swap(true, Ordering::SeqCst) {
                        let _ = ev.try_send(AudioEvent::Failed(why));
                    }
                    ml.quit();
                }
            })
            .process(move |stream, ud| process(stream, ud, output))
            .register()
            .map_err(|e| format!("stream listener: {e}"))?
    };

    let values = format_pod()?;
    let mut params = [spa::pod::Pod::from_bytes(&values).ok_or("format pod")?];
    let mut flags = pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;
    if matches!(kind, Kind::Capture | Kind::Playback) {
        flags |= pw::stream::StreamFlags::AUTOCONNECT;
    }
    let direction = if output {
        spa::utils::Direction::Output
    } else {
        spa::utils::Direction::Input
    };
    stream
        .connect(direction, None, flags, &mut params)
        .map_err(|e| format!("PipeWire stream connect: {e}"))?;

    let _quit = {
        let ml = mainloop.clone();
        quit_rx.attach(mainloop.loop_(), move |()| ml.quit())
    };

    // Make sure the server accepted everything before reporting success.
    let pending = core.sync(0).map_err(|e| format!("PipeWire sync: {e}"))?;
    let done = Arc::new(AtomicBool::new(false));
    let _sync_listener = {
        let done = done.clone();
        let ml = mainloop.clone();
        core.add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.store(true, Ordering::SeqCst);
                    ml.quit();
                }
            })
            .register()
    };
    mainloop.run(); // returns on sync done, fatal error or quit
    if !done.load(Ordering::SeqCst) {
        return Err("PipeWire refused the stream".into());
    }
    let _ = ready.send(Ok(()));
    if !failed.load(Ordering::SeqCst) {
        mainloop.run(); // until dropped (quit) or failure
    }
    // Our own shutdown: the disconnect below is not a failure.
    failed.store(true, Ordering::SeqCst);
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_pod_serializes() {
        let v = format_pod().unwrap();
        assert!(spa::pod::Pod::from_bytes(&v).is_some());
    }

    #[test]
    fn virtual_nodes_are_named_and_not_autoconnected() {
        let spec = AudioSpec {
            device: Some("linker_microphone".into()),
            description: "Linker Microphone".into(),
            period_ms: 2.67,
        };
        let p = props_for(Kind::VirtualMic, &spec);
        assert_eq!(p.get("media.class"), Some("Audio/Source"));
        assert_eq!(p.get("node.description"), Some("Linker Microphone"));
        assert_eq!(p.get("node.autoconnect"), Some("false"));
        assert_eq!(p.get("node.latency"), Some("128/48000"));
        let p = props_for(Kind::VirtualSpeaker, &spec);
        assert_eq!(p.get("media.class"), Some("Audio/Sink"));
        let p = props_for(
            Kind::Capture,
            &AudioSpec {
                device: Some("linker_microphone".into()),
                ..Default::default()
            },
        );
        assert_eq!(p.get("target.object"), Some("linker_microphone"));
        assert_eq!(p.get("stream.capture.sink"), None);
        let p = props_for(
            Kind::Capture,
            &AudioSpec {
                device: Some("alsa_output.pci.analog-stereo.monitor".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            p.get("target.object"),
            Some("alsa_output.pci.analog-stereo")
        );
        assert_eq!(p.get("stream.capture.sink"), Some("true"));
    }
}
