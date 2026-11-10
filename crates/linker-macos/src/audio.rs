//! Audio: microphone capture and speaker playback through CoreAudio (`cpal`).
//!
//! * All PCM crossing the platform API is 48 kHz interleaved stereo `i16`.
//!   Devices are opened at their **default** (nominal) rate and channel
//!   count — changing a device's rate would affect every other app — and
//!   converted here: any sample format / channel count ↔ stereo `i16`, then
//!   linear resampling ([`Resampler`] for capture, [`PullResampler`] for
//!   playback) when the device does not run at 48 kHz. cpal on macOS only
//!   reports F32/I16 virtual formats; any other one is opened as F32 at the
//!   device's nominal rate (the AudioUnit converts).
//! * cpal sets the device's *physical* format when it opens a stream (e.g.
//!   a 24-bit integer interface may be switched to 32-bit float). The
//!   original physical format is restored when Linker's last stream on that
//!   device closes, unless something else changed it in the meantime.
//! * Low latency playback like the Go audiobridge player (malgo
//!   `LowLatency`, `NoFixedSizedCallback`): a fixed IO buffer of
//!   `round(rate × period_ms / 1000)` frames (≈128 at 48 kHz), clamped into
//!   the device's supported range; the PCM callback runs directly in the
//!   CoreAudio IO callback. Capture keeps the device's default IO size.
//! * Real-time safe callbacks: scratch buffers are preallocated (and grow at
//!   most once if CoreAudio hands a larger buffer), no locks, no logging.
//! * `cpal::Stream` is built, started and owned by a dedicated thread
//!   (`linker-audio-in` / `linker-audio-out`); the returned handle stops and
//!   joins it on drop. The same thread polls the system default device every
//!   2 s (when no device was named) and reports a change as
//!   [`AudioEvent::Failed`] so the core reopens on the new default.
//! * The microphone is only opened after the TCC permission check
//!   (`AVCaptureDevice authorizationStatusForMediaType: AVMediaTypeAudio`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, ErrorKind, FromSample, Host, InputCallbackInfo, OutputCallbackInfo, Sample,
    SampleFormat, SizedSample, StreamConfig, SupportedBufferSize, SupportedStreamConfig,
};
use linker_audio::frame::{
    from_stereo_i16_f32, from_stereo_i16_i16, to_stereo_i16_f32, to_stereo_i16_i16, PullResampler,
    Resampler,
};
use linker_platform::{
    AudioEvent, AudioSpec, AudioStream, AudioStreamInfo, Cap, PcmSink, PcmSource, Unsupported,
    AUDIO_CHANNELS, AUDIO_RATE,
};
use tokio::sync::mpsc;

/// How often the default device is polled for changes.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);
/// Initial scratch capacity in frames (grows once if a callback is larger).
const SCRATCH_FRAMES: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Input,
    Output,
}

impl Dir {
    fn label(self) -> &'static str {
        match self {
            Dir::Input => "input",
            Dir::Output => "output",
        }
    }
}

/// Lock-free statistics written by the audio callback.
struct Stats {
    /// Origin of the callback wall clock.
    base: Instant,
    /// `f64` bits, ms.
    latency_ms: AtomicU64,
    /// Wall-clock ns (since `base`) of the previous callback (0 = none yet).
    last_cb_ns: AtomicU64,
    max_gap_ns: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            latency_ms: AtomicU64::new(0),
            last_cb_ns: AtomicU64::new(0),
            max_gap_ns: AtomicU64::new(0),
        }
    }

    /// Records one callback. The gap is measured on the wall clock at the
    /// moment the callback *runs* (like the Go player's `time.Now()`), not
    /// from cpal's `timestamp().callback`: that is the IO cycle's
    /// `mHostTime`, which advances one period per cycle however late the
    /// callback thread is scheduled. `Instant::now()` is
    /// `mach_absolute_time`-based on macOS — no allocation, no syscall lock.
    /// cpal's timestamps are used only for the device latency.
    #[inline]
    fn on_callback(&self, latency: Option<Duration>) {
        let now_ns = self.base.elapsed().as_nanos() as u64;
        let last = self.last_cb_ns.swap(now_ns.max(1), Ordering::Relaxed);
        if last != 0 && now_ns > last {
            self.max_gap_ns.fetch_max(now_ns - last, Ordering::Relaxed);
        }
        if let Some(d) = latency {
            self.latency_ms
                .store((d.as_secs_f64() * 1000.0).to_bits(), Ordering::Relaxed);
        }
    }
}

/// Reports a stream failure at most once, never blocking.
#[derive(Clone)]
struct Reporter {
    tx: mpsc::Sender<AudioEvent>,
    sent: Arc<AtomicBool>,
}

impl Reporter {
    fn fail(&self, msg: String) {
        if self.sent.load(Ordering::Relaxed) {
            return;
        }
        if self.tx.try_send(AudioEvent::Failed(msg)).is_ok() {
            self.sent.store(true, Ordering::Relaxed);
        }
    }
}

/// A running capture or playback stream. Dropping it stops the stream.
pub struct MacAudioStream {
    info: AudioStreamInfo,
    stats: Arc<Stats>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioStream for MacAudioStream {
    fn info(&self) -> AudioStreamInfo {
        let mut info = self.info.clone();
        info.device_latency_ms = f64::from_bits(self.stats.latency_ms.load(Ordering::Relaxed));
        info
    }
    fn take_max_callback_gap_ms(&self) -> f64 {
        self.stats.max_gap_ns.swap(0, Ordering::Relaxed) as f64 / 1e6
    }
}

impl Drop for MacAudioStream {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Microphone capture: `sink` receives 48 kHz stereo `i16`.
pub fn capture(
    spec: AudioSpec,
    sink: PcmSink,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    authorize_microphone()?;
    start(Dir::Input, spec, Callback::Capture(sink), ev)
}

/// Speaker playback: `src` is pulled for 48 kHz stereo `i16`.
pub fn playback(
    spec: AudioSpec,
    src: PcmSource,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    start(Dir::Output, spec, Callback::Playback(src), ev)
}

fn authorize_microphone() -> Cap<()> {
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: framework-exported constant NSString.
        let media = unsafe { crate::objc::AVMediaTypeAudio };
        crate::camera::authorize(
            media,
            "microphone access was not granted",
            "microphone permission denied — System Settings → Privacy & Security → Microphone → enable linkerd",
        )
    })
}

enum Callback {
    Capture(PcmSink),
    Playback(PcmSource),
}

fn start(
    dir: Dir,
    spec: AudioSpec,
    cb: Callback,
    ev: mpsc::Sender<AudioEvent>,
) -> Cap<Box<dyn AudioStream>> {
    let stats = Arc::new(Stats::new());
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<AudioStreamInfo, String>>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let name = match dir {
        Dir::Input => "linker-audio-in",
        Dir::Output => "linker-audio-out",
    };
    let thread_stats = stats.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || run(dir, spec, cb, ev, thread_stats, ready_tx, stop_rx))
        .map_err(|e| Unsupported::new(format!("cannot spawn {name}: {e}")))?;
    match ready_rx.recv() {
        Ok(Ok(info)) => Ok(Box::new(MacAudioStream {
            info,
            stats,
            stop: Some(stop_tx),
            thread: Some(thread),
        })),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(Unsupported::new(e))
        }
        Err(_) => {
            let _ = thread.join();
            Err(Unsupported::new(format!("{name} exited during setup")))
        }
    }
}

/// Body of the stream-owning thread.
fn run(
    dir: Dir,
    spec: AudioSpec,
    cb: Callback,
    ev: mpsc::Sender<AudioEvent>,
    stats: Arc<Stats>,
    ready: std::sync::mpsc::Sender<Result<AudioStreamInfo, String>>,
    stop: std::sync::mpsc::Receiver<()>,
) {
    let host = cpal::default_host();
    let rep = Reporter {
        tx: ev,
        sent: Arc::new(AtomicBool::new(false)),
    };
    let Opened {
        device,
        stream,
        info,
        phys,
    } = match open(&host, dir, &spec, cb, &stats, &rep) {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    tracing::info!(
        dir = dir.label(),
        device = %info.device,
        rate = info.sample_rate,
        channels = info.channels,
        period = info.period_frames,
        "audio stream started"
    );
    if ready.send(Ok(info)).is_err() {
        drop(stream);
        drop(phys);
        return;
    }
    // Runs until stop is requested or the handle is dropped.
    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(WATCH_INTERVAL) {
        if spec.device.is_some() {
            continue;
        }
        if let Some(new) = default_device(&host, dir) {
            if new != device {
                rep.fail(format!(
                    "default {} changed to {}",
                    dir.label(),
                    device_name(&new)
                ));
            }
        }
    }
    let _ = stream.pause();
    drop(stream);
    drop(phys); // restores the physical format once the stream is gone
}

fn default_device(host: &Host, dir: Dir) -> Option<Device> {
    match dir {
        Dir::Input => host.default_input_device(),
        Dir::Output => host.default_output_device(),
    }
}

fn device_name(d: &Device) -> String {
    d.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown device>".into())
}

/// Default device, or the one named `want` (exact name, then
/// case-insensitive substring).
fn select_device(host: &Host, dir: Dir, want: Option<&str>) -> Result<Device, String> {
    let Some(want) = want else {
        return default_device(host, dir)
            .ok_or_else(|| format!("no default audio {}", dir.label()));
    };
    let devices: Vec<Device> = match dir {
        Dir::Input => host.input_devices().map(|d| d.collect()),
        Dir::Output => host.output_devices().map(|d| d.collect()),
    }
    .map_err(|e| format!("cannot list audio {} devices: {e}", dir.label()))?;
    let named: Vec<(String, Device)> = devices.into_iter().map(|d| (device_name(&d), d)).collect();
    if let Some((_, d)) = named.iter().find(|(n, _)| n == want) {
        return Ok(d.clone());
    }
    let lw = want.to_lowercase();
    if let Some((_, d)) = named.iter().find(|(n, _)| n.to_lowercase().contains(&lw)) {
        return Ok(d.clone());
    }
    let list: Vec<&str> = named.iter().map(|(n, _)| n.as_str()).collect();
    Err(format!(
        "audio {} device \"{want}\" not found; available: {}",
        dir.label(),
        if list.is_empty() {
            "(none)".to_string()
        } else {
            list.join(", ")
        }
    ))
}

/// Fixed buffer of `round(rate × period_ms / 1000)` frames, clamped into the
/// device's supported range; `Default` when the range is unknown.
fn pick_buffer(rate: u32, period_ms: f64, supported: &SupportedBufferSize) -> BufferSize {
    let want = (rate as f64 * period_ms / 1000.0).round().max(1.0) as u32;
    match *supported {
        SupportedBufferSize::Range { min, max } if min <= max && max > 0 => {
            BufferSize::Fixed(want.clamp(min.max(1), max))
        }
        _ => BufferSize::Default,
    }
}

fn open(
    host: &Host,
    dir: Dir,
    spec: &AudioSpec,
    cb: Callback,
    stats: &Arc<Stats>,
    rep: &Reporter,
) -> Result<Opened, String> {
    let device = select_device(host, dir, spec.device.as_deref())?;
    let name = device_name(&device);
    let hal_id = ca::device_id(&device);
    let sup = default_config(&device, dir, hal_id)
        .map_err(|e| format!("audio {} \"{name}\": no default config: {e}", dir.label()))?;
    let rate = sup.sample_rate();
    let channels = sup.channels();
    if rate == 0 || channels == 0 {
        return Err(format!(
            "audio {} \"{name}\": invalid default config ({rate} Hz, {channels} ch)",
            dir.label()
        ));
    }
    let period_ms = if spec.period_ms > 0.0 {
        spec.period_ms
    } else {
        AudioSpec::default().period_ms
    };
    // Low-latency fixed buffer for playback only (the Go player's
    // `LowLatency` profile). Capture keeps the device's default IO size: the
    // core gathers 20 ms Opus frames anyway, so 128-frame mic callbacks would
    // only multiply real-time wakeups.
    let buffer = match dir {
        Dir::Output => pick_buffer(rate, period_ms, sup.buffer_size()),
        Dir::Input => BufferSize::Default,
    };
    // cpal switches the device's physical format when it builds a stream;
    // put it back when Linker's last stream on the device closes.
    let phys = hal_id.and_then(ca::PhysicalFormatGuard::acquire);

    // The callbacks are moved into cpal; keep them in a slot so a failed
    // fixed-buffer attempt can be retried with the default buffer.
    let mut cb = Some(cb);
    let mut try_build = |buffer: BufferSize| -> Result<cpal::Stream, String> {
        let config = StreamConfig {
            channels,
            sample_rate: rate,
            buffer_size: buffer,
        };
        let c = cb.take().ok_or("callback lost after a failed attempt")?;
        build(&device, &sup, config, c, stats.clone(), rep.clone()).map_err(|(e, c)| {
            cb = c;
            e
        })
    };
    let mut used = buffer;
    let stream = match try_build(buffer) {
        Ok(s) => Ok(s),
        Err(e) if buffer != BufferSize::Default => {
            tracing::debug!(error = %e, ?buffer, "fixed audio buffer refused; using default");
            used = BufferSize::Default;
            try_build(BufferSize::Default)
        }
        Err(e) => Err(e),
    }
    .map_err(|e| format!("audio {} \"{name}\": {e}", dir.label()))?;
    if let Err(e) = stream.play() {
        drop(stream); // before `phys`
        return Err(format!(
            "audio {} \"{name}\": start failed: {e}",
            dir.label()
        ));
    }
    if let Some(g) = &phys {
        g.opened();
    }
    let period_frames = stream.buffer_size().ok().unwrap_or(match used {
        BufferSize::Fixed(n) => n,
        BufferSize::Default => 0,
    });
    let info = AudioStreamInfo {
        device: name,
        sample_rate: rate,
        channels,
        period_frames,
        device_latency_ms: 0.0,
    };
    Ok(Opened {
        device,
        stream,
        info,
        phys,
    })
}

struct Opened {
    device: Device,
    stream: cpal::Stream,
    info: AudioStreamInfo,
    /// Dropped after `stream`.
    phys: Option<ca::PhysicalFormatGuard>,
}

/// The device's default (virtual) config. On macOS cpal only accepts an
/// F32 or I16 virtual format and errors otherwise (e.g. a 24/32-bit integer
/// stream on a pro interface); then open it as F32 at the device's current
/// nominal rate — the AudioUnit converts — rather than failing.
fn default_config(
    device: &Device,
    dir: Dir,
    hal_id: Option<objc2_core_audio::AudioObjectID>,
) -> Result<SupportedStreamConfig, String> {
    let def = match dir {
        Dir::Input => device.default_input_config(),
        Dir::Output => device.default_output_config(),
    };
    let err = match def {
        Ok(c) => return Ok(c),
        Err(e) if e.kind() == ErrorKind::UnsupportedConfig => e,
        Err(e) => return Err(e.to_string()),
    };
    let rate = hal_id
        .and_then(ca::nominal_rate)
        .ok_or_else(|| format!("{err} (and the nominal rate is unreadable)"))?;
    let range = match dir {
        Dir::Input => device.supported_input_configs().map(|mut c| c.next()),
        Dir::Output => device.supported_output_configs().map(|mut c| c.next()),
    }
    .map_err(|e| format!("{err}; {e}"))?
    .ok_or_else(|| format!("{err} (no supported configs)"))?;
    Ok(SupportedStreamConfig::new(
        range.channels(),
        rate,
        *range.buffer_size(),
        SampleFormat::F32,
    ))
}

/// On failure the callback is handed back (when recoverable) for a retry.
type BuildResult = Result<cpal::Stream, (String, Option<Callback>)>;

/// Dispatch on the device's sample format.
fn build(
    device: &Device,
    sup: &SupportedStreamConfig,
    config: StreamConfig,
    cb: Callback,
    stats: Arc<Stats>,
    rep: Reporter,
) -> BuildResult {
    macro_rules! go {
        ($t:ty, $in_conv:expr, $out_conv:expr) => {
            match cb {
                Callback::Capture(sink) => {
                    build_input::<$t>(device, config, sink, $in_conv, stats, rep)
                        .map_err(|(e, s)| (e, s.map(Callback::Capture)))
                }
                Callback::Playback(src) => {
                    build_output::<$t>(device, config, src, $out_conv, stats, rep)
                        .map_err(|(e, s)| (e, s.map(Callback::Playback)))
                }
            }
        };
    }
    use cpal::{I24, U24};
    match sup.sample_format() {
        SampleFormat::I16 => go!(i16, to_stereo_i16_i16, from_stereo_i16_i16),
        SampleFormat::F32 => go!(f32, to_stereo_i16_f32, from_stereo_i16_f32),
        SampleFormat::I8 => go!(i8, in_generic::<i8>, out_generic::<i8>),
        SampleFormat::I24 => go!(I24, in_generic::<I24>, out_generic::<I24>),
        SampleFormat::I32 => go!(i32, in_generic::<i32>, out_generic::<i32>),
        SampleFormat::I64 => go!(i64, in_generic::<i64>, out_generic::<i64>),
        SampleFormat::U8 => go!(u8, in_generic::<u8>, out_generic::<u8>),
        SampleFormat::U16 => go!(u16, in_generic::<u16>, out_generic::<u16>),
        SampleFormat::U24 => go!(U24, in_generic::<U24>, out_generic::<U24>),
        SampleFormat::U32 => go!(u32, in_generic::<u32>, out_generic::<u32>),
        SampleFormat::U64 => go!(u64, in_generic::<u64>, out_generic::<u64>),
        SampleFormat::F64 => go!(f64, in_generic::<f64>, out_generic::<f64>),
        other => Err((format!("unsupported sample format {other}"), Some(cb))),
    }
}

/// Device samples (`channels` interleaved) → stereo `i16`, appended.
type InConv<T> = fn(&[T], usize, &mut Vec<i16>);
/// Stereo `i16` → device samples (`channels` interleaved), written.
type OutConv<T> = fn(&[i16], &mut [T], usize);

fn in_generic<T: Sample>(input: &[T], channels: usize, out: &mut Vec<i16>)
where
    i16: FromSample<T>,
{
    if channels == 0 {
        return;
    }
    out.reserve(input.len() / channels * AUDIO_CHANNELS);
    for fr in input.chunks_exact(channels) {
        let l: i16 = fr[0].to_sample();
        let r: i16 = if channels == 1 { l } else { fr[1].to_sample() };
        out.push(l);
        out.push(r);
    }
}

fn out_generic<T: Sample + FromSample<i16>>(input: &[i16], out: &mut [T], channels: usize) {
    if channels == 0 {
        return;
    }
    for (src, dst) in input
        .as_chunks::<AUDIO_CHANNELS>()
        .0
        .iter()
        .zip(out.chunks_exact_mut(channels))
    {
        if channels == 1 {
            dst[0] = T::from_sample(((src[0] as i32 + src[1] as i32) / 2) as i16);
        } else {
            dst[0] = T::from_sample(src[0]);
            dst[1] = T::from_sample(src[1]);
            dst[2..].fill(T::EQUILIBRIUM);
        }
    }
}

/// cpal error callback: overruns / realtime refusals are not fatal; a
/// reroute of the default device or any real error makes the owner reopen.
fn on_error(dir: Dir, rep: Reporter) -> impl FnMut(cpal::Error) + Send + 'static {
    move |e: cpal::Error| match e.kind() {
        ErrorKind::Xrun | ErrorKind::RealtimeDenied => {}
        ErrorKind::DeviceChanged => rep.fail(format!("default {} changed (rerouted)", dir.label())),
        _ => rep.fail(format!("audio {} error: {e}", dir.label())),
    }
}

fn build_input<T: SizedSample + Send + 'static>(
    device: &Device,
    config: StreamConfig,
    sink: PcmSink,
    conv: InConv<T>,
    stats: Arc<Stats>,
    rep: Reporter,
) -> Result<cpal::Stream, (String, Option<PcmSink>)> {
    let channels = config.channels as usize;
    let mut rs = Resampler::new(config.sample_rate);
    let mut stereo: Vec<i16> = Vec::with_capacity(SCRATCH_FRAMES * AUDIO_CHANNELS);
    let mut resampled: Vec<i16> = Vec::with_capacity(
        SCRATCH_FRAMES * AUDIO_CHANNELS * AUDIO_RATE as usize / config.sample_rate.max(1) as usize
            + 2 * AUDIO_CHANNELS,
    );
    // The sink lives in a slot until the first callback takes it, so that a
    // failed build (retried with another buffer size) can hand it back.
    // The callback only ever uses `try_lock` (never blocks), once.
    let slot = Arc::new(std::sync::Mutex::new(Some(sink)));
    let cb_slot = slot.clone();
    let mut owned: Option<PcmSink> = None;
    let data_cb = move |data: &[T], info: &InputCallbackInfo| {
        let ts = info.timestamp();
        stats.on_callback(ts.callback.checked_duration_since(ts.capture));
        if owned.is_none() {
            if let Ok(mut g) = cb_slot.try_lock() {
                owned = g.take();
            }
        }
        let Some(sink) = owned.as_mut() else { return };
        stereo.clear();
        conv(data, channels, &mut stereo);
        if rs.is_passthrough() {
            if !stereo.is_empty() {
                sink(&stereo);
            }
        } else {
            resampled.clear();
            rs.process(&stereo, &mut resampled);
            if !resampled.is_empty() {
                sink(&resampled);
            }
        }
    };
    device
        .build_input_stream::<T, _, _>(config, data_cb, on_error(Dir::Input, rep), None)
        .map_err(|e| (e.to_string(), take_back(&slot)))
}

/// After a failed build the data callback was dropped without running:
/// the user closure is still in its slot.
fn take_back<T>(slot: &std::sync::Mutex<Option<T>>) -> Option<T> {
    slot.lock().ok().and_then(|mut g| g.take())
}

fn build_output<T: SizedSample + Send + 'static>(
    device: &Device,
    config: StreamConfig,
    src: PcmSource,
    conv: OutConv<T>,
    stats: Arc<Stats>,
    rep: Reporter,
) -> Result<cpal::Stream, (String, Option<PcmSource>)> {
    let channels = config.channels as usize;
    let mut pr = PullResampler::new(config.sample_rate);
    let mut stereo: Vec<i16> = Vec::with_capacity(SCRATCH_FRAMES * AUDIO_CHANNELS);
    let slot = Arc::new(std::sync::Mutex::new(Some(src)));
    let cb_slot = slot.clone();
    let mut owned: Option<PcmSource> = None;
    let data_cb = move |data: &mut [T], info: &OutputCallbackInfo| {
        let ts = info.timestamp();
        stats.on_callback(ts.playback.checked_duration_since(ts.callback));
        if owned.is_none() {
            if let Ok(mut g) = cb_slot.try_lock() {
                owned = g.take();
            }
        }
        // `data` is pre-filled with silence.
        let Some(src) = owned.as_mut() else { return };
        let frames = data.len() / channels.max(1);
        // Within capacity after warm-up: no allocation.
        stereo.resize(frames * AUDIO_CHANNELS, 0);
        if pr.is_passthrough() {
            src(&mut stereo);
        } else {
            pr.fill(&mut stereo, |b| src(b));
        }
        conv(&stereo, data, channels);
    };
    device
        .build_output_stream::<T, _, _>(config, data_cb, on_error(Dir::Output, rep), None)
        .map_err(|e| (e.to_string(), take_back(&slot)))
}

/// Minimal CoreAudio HAL access for what `cpal` does not expose: the
/// device's nominal rate (fallback when `cpal` rejects the virtual format)
/// and the physical stream format, which `cpal` switches when it opens a
/// stream and never puts back.
mod ca {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::ptr::{null, NonNull};
    use std::sync::Mutex;

    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use cpal::traits::DeviceTrait;
    use objc2_core_audio::{
        kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
        kAudioStreamPropertyPhysicalFormat, AudioObjectGetPropertyData, AudioObjectID,
        AudioObjectPropertyAddress, AudioObjectPropertySelector, AudioObjectSetPropertyData,
    };
    use objc2_core_audio_types::AudioStreamBasicDescription;

    const UNKNOWN: AudioObjectID = 0; // kAudioObjectUnknown

    fn global(sel: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: sel,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    /// Reads a fixed-size property.
    ///
    /// # Safety
    /// `T` must be the property's documented C type.
    unsafe fn get<T>(
        obj: AudioObjectID,
        sel: AudioObjectPropertySelector,
        qualifier: Option<(&[u8], *const c_void)>,
        out: &mut T,
    ) -> bool {
        let addr = global(sel);
        let mut size = std::mem::size_of::<T>() as u32;
        let (qs, qp) = qualifier.map_or((0, null()), |(b, p)| (b.len() as u32, p));
        let st = unsafe {
            AudioObjectGetPropertyData(
                obj,
                NonNull::from(&addr),
                qs,
                qp,
                NonNull::from(&mut size),
                NonNull::from(out).cast(),
            )
        };
        st == 0 && size as usize == std::mem::size_of::<T>()
    }

    /// HAL object id of a `cpal` device (via its UID).
    pub(super) fn device_id(dev: &cpal::Device) -> Option<AudioObjectID> {
        let uid = dev.id().ok()?;
        let cf = CFString::new(uid.id());
        let r: CFStringRef = cf.as_concrete_TypeRef();
        let mut id: AudioObjectID = UNKNOWN;
        let q = std::mem::size_of::<CFStringRef>();
        // SAFETY: the qualifier is a CFStringRef (kept alive by `cf`), the
        // result an AudioObjectID, as documented for TranslateUIDToDevice.
        let ok = unsafe {
            get(
                kAudioObjectSystemObject as AudioObjectID,
                kAudioHardwarePropertyTranslateUIDToDevice,
                Some((&[0u8; 8][..q], (&r as *const CFStringRef).cast())),
                &mut id,
            )
        };
        (ok && id != UNKNOWN).then_some(id)
    }

    /// The device's current nominal sample rate.
    pub(super) fn nominal_rate(id: AudioObjectID) -> Option<u32> {
        let mut rate = 0f64;
        // SAFETY: NominalSampleRate is a Float64.
        let ok = unsafe { get(id, kAudioDevicePropertyNominalSampleRate, None, &mut rate) };
        (ok && rate >= 1.0).then(|| rate.round() as u32)
    }

    /// Physical format, addressed exactly as `cpal`/`coreaudio-rs` set it
    /// (stream property on the device object, global scope).
    fn physical_format(id: AudioObjectID) -> Option<AudioStreamBasicDescription> {
        // SAFETY: all-zero is a valid AudioStreamBasicDescription.
        let mut asbd: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
        // SAFETY: PhysicalFormat is an AudioStreamBasicDescription.
        let ok = unsafe { get(id, kAudioStreamPropertyPhysicalFormat, None, &mut asbd) };
        ok.then_some(asbd)
    }

    fn set_physical_format(id: AudioObjectID, asbd: &AudioStreamBasicDescription) -> bool {
        let addr = global(kAudioStreamPropertyPhysicalFormat);
        // SAFETY: PhysicalFormat takes an AudioStreamBasicDescription.
        let st = unsafe {
            AudioObjectSetPropertyData(
                id,
                NonNull::from(&addr),
                0,
                null(),
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
                NonNull::from(asbd).cast(),
            )
        };
        st == 0
    }

    fn same(a: &AudioStreamBasicDescription, b: &AudioStreamBasicDescription) -> bool {
        a.mSampleRate.to_bits() == b.mSampleRate.to_bits()
            && a.mFormatID == b.mFormatID
            && a.mFormatFlags == b.mFormatFlags
            && a.mBytesPerPacket == b.mBytesPerPacket
            && a.mFramesPerPacket == b.mFramesPerPacket
            && a.mBytesPerFrame == b.mBytesPerFrame
            && a.mChannelsPerFrame == b.mChannelsPerFrame
            && a.mBitsPerChannel == b.mBitsPerChannel
    }

    struct Saved {
        users: usize,
        /// Format before Linker's first stream on this device.
        original: AudioStreamBasicDescription,
        /// Format once that stream was open (what `cpal` switched it to).
        ours: Option<AudioStreamBasicDescription>,
    }

    // SAFETY: plain C struct of numbers.
    unsafe impl Send for Saved {}

    static SAVED: Mutex<Option<HashMap<AudioObjectID, Saved>>> = Mutex::new(None);

    /// Remembers a device's physical format before `cpal` opens a stream on
    /// it and restores it when the last Linker stream on that device
    /// closes — unless someone else changed it meanwhile.
    pub(super) struct PhysicalFormatGuard {
        id: AudioObjectID,
    }

    impl PhysicalFormatGuard {
        /// Call before building the stream.
        pub(super) fn acquire(id: AudioObjectID) -> Option<Self> {
            let mut g = SAVED.lock().ok()?;
            let map = g.get_or_insert_with(HashMap::new);
            if let Some(s) = map.get_mut(&id) {
                s.users += 1;
            } else {
                let original = physical_format(id)?;
                map.insert(
                    id,
                    Saved {
                        users: 1,
                        original,
                        ours: None,
                    },
                );
            }
            Some(Self { id })
        }

        /// Call once the stream is running.
        pub(super) fn opened(&self) {
            let Ok(mut g) = SAVED.lock() else { return };
            if let Some(s) = g.as_mut().and_then(|m| m.get_mut(&self.id)) {
                if s.ours.is_none() {
                    s.ours = physical_format(self.id);
                }
            }
        }
    }

    impl Drop for PhysicalFormatGuard {
        /// Run after the stream has been dropped.
        fn drop(&mut self) {
            let Ok(mut g) = SAVED.lock() else { return };
            let Some(map) = g.as_mut() else { return };
            let Some(s) = map.get_mut(&self.id) else {
                return;
            };
            s.users -= 1;
            if s.users > 0 {
                return;
            }
            let Some(s) = map.remove(&self.id) else {
                return;
            };
            let Some(now) = physical_format(self.id) else {
                return;
            };
            // `ours` is unknown when building/starting the stream failed —
            // cpal may already have switched the format before failing. The
            // window since `acquire` is tiny, so a changed format is ours.
            let ours = s.ours.unwrap_or(now);
            if !same(&s.original, &ours) && same(&now, &ours) {
                if set_physical_format(self.id, &s.original) {
                    tracing::debug!(device = self.id, "restored audio physical format");
                } else {
                    tracing::warn!(device = self.id, "could not restore audio physical format");
                }
            }
        }
    }
}
