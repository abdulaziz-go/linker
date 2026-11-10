//! The [`PlatformBackend`] trait: everything OS-specific that the
//! platform-independent core needs, plus shared helpers.
//!
//! Every capability has a default implementation that returns
//! [`Unsupported`], so a backend only implements what its OS can do and the
//! corresponding feature simply doesn't activate elsewhere.

pub mod echo;
pub mod hash;

use std::path::PathBuf;

pub use linker_proto::{CamState, Edge, MouseButton, Notification};
use tokio::sync::mpsc;

/// A capability is not available on this platform / session / permission set.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unsupported: {0}")]
pub struct Unsupported(pub String);

impl Unsupported {
    pub fn new(msg: impl Into<String>) -> Self {
        Unsupported(msg.into())
    }
}

impl From<anyhow::Error> for Unsupported {
    fn from(e: anyhow::Error) -> Self {
        Unsupported(format!("{e:#}"))
    }
}

pub type Cap<T> = Result<T, Unsupported>;

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardContent {
    Text {
        text: String,
        html: Option<String>,
    },
    /// PNG-encoded image.
    Image {
        png: Vec<u8>,
    },
    /// Local file paths.
    Files {
        paths: Vec<PathBuf>,
    },
}

#[derive(Clone, Debug)]
pub struct ClipboardChange {
    pub content: ClipboardContent,
    /// Marked concealed/transient by the source app (e.g. password managers).
    pub concealed: bool,
}

// ---------------------------------------------------------------------------
// Power
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    /// The machine is about to sleep.
    Sleeping,
    /// The machine woke up.
    Woke,
}

// ---------------------------------------------------------------------------
// Input (KVM)
// ---------------------------------------------------------------------------

/// An event read from the local physical keyboard / mouse.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LocalInput {
    /// Relative motion. `pos` is the absolute cursor position after the
    /// motion when the platform knows it (macOS); `None` means the core must
    /// track a virtual cursor from deltas (Linux / Wayland).
    Motion {
        dx: i32,
        dy: i32,
        pos: Option<(f64, f64)>,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    /// Scroll in lines; positive dy = up.
    Scroll {
        dx: i32,
        dy: i32,
    },
    /// Key using HID usage code.
    Key {
        hid: u16,
        pressed: bool,
    },
}

/// Local input capture (server role).
pub trait InputCapture: Send {
    /// While captured, local input is suppressed from the local desktop and
    /// only delivered to the core (EVIOCGRAB / event-tap swallow).
    fn set_captured(&mut self, captured: bool) -> anyhow::Result<()>;
    /// Local screen size in pixels (the logical desktop used for edge detection).
    fn screen_size(&self) -> (f64, f64);
    /// Best-effort: put the local cursor at `edge`, `ratio` along it.
    fn warp_to(&mut self, edge: Edge, ratio: f64) -> anyhow::Result<()>;
}

/// Input injection (client role).
pub trait InputInjector: Send {
    fn move_relative(&mut self, dx: i32, dy: i32) -> anyhow::Result<()>;
    fn button(&mut self, button: MouseButton, pressed: bool) -> anyhow::Result<()>;
    fn scroll(&mut self, dx: i32, dy: i32) -> anyhow::Result<()>;
    fn key(&mut self, hid: u16, pressed: bool) -> anyhow::Result<()>;
    /// Place the cursor on `edge` at `ratio` (when control enters this screen).
    fn warp_to(&mut self, edge: Edge, ratio: f64) -> anyhow::Result<()>;
    /// Current cursor position, if the platform can report it. If `None`
    /// the core tracks a virtual cursor from the deltas it injected.
    fn cursor_position(&self) -> Option<(f64, f64)>;
    fn screen_size(&self) -> (f64, f64);
}

// ---------------------------------------------------------------------------
// Camera
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// One H.264 access unit in Annex-B format (start codes, SPS/PPS inline
/// before every IDR).
#[derive(Clone, Debug)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Capture time on the source's wall clock, µs since UNIX epoch.
    pub capture_ts_us: u64,
}

#[derive(Clone, Debug)]
pub enum CameraEvent {
    Frame(EncodedFrame),
    State { state: CamState, detail: String },
}

/// Running camera capture+encode pipeline (source role). Dropping it stops
/// capture and releases the camera.
pub trait CameraSource: Send {
    fn request_keyframe(&self);
    fn set_bitrate(&self, kbps: u32);
}

/// Virtual-webcam output (sink role). Methods may block (decode + write);
/// the core calls them from a dedicated thread.
pub trait CameraSink: Send {
    /// Decode one Annex-B access unit and write the picture to the virtual
    /// device. Returns `Ok(true)` if a picture was written.
    fn write_encoded(&mut self, annexb: &[u8]) -> anyhow::Result<bool>;
    /// Write a placeholder frame (no stream / camera busy / link down).
    fn write_placeholder(&mut self) -> anyhow::Result<()>;
    /// Number of *other* processes that currently have the device open.
    fn consumers(&self) -> usize;
    /// Forget decoder state (after loss / stream restart).
    fn reset_decoder(&mut self);
    /// e.g. `/dev/video10`
    fn device_path(&self) -> String;
    /// Human readable decoder description (e.g. "openh264 (software)").
    fn decoder_name(&self) -> String;
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

/// Sample rate of all PCM crossing the audio API.
pub const AUDIO_RATE: u32 = 48_000;
/// Interleaved channels of all PCM crossing the audio API.
pub const AUDIO_CHANNELS: usize = 2;

/// Which device/node to open and how.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSpec {
    /// Physical device (macOS: device name; Linux: PipeWire `target.object`)
    /// or, for virtual devices, the PipeWire `node.name`. `None` = default.
    pub device: Option<String>,
    /// Human readable name of a virtual device ("Linker Microphone").
    pub description: String,
    /// Requested callback period in ms (2.67 ≈ 128 frames at 48 kHz).
    pub period_ms: f64,
}

impl Default for AudioSpec {
    fn default() -> Self {
        AudioSpec {
            device: None,
            description: String::new(),
            period_ms: 2.67,
        }
    }
}

/// Receives captured PCM: 48 kHz interleaved stereo `i16`, any length.
/// Called on the audio thread — must not block.
pub type PcmSink = Box<dyn FnMut(&[i16]) + Send>;
/// Fills a playback buffer completely with 48 kHz interleaved stereo `i16`.
/// Called on the real-time audio thread — must not block.
pub type PcmSource = Box<dyn FnMut(&mut [i16]) + Send>;

#[derive(Clone, Debug, PartialEq)]
pub enum AudioEvent {
    /// The stream died (device removed/changed, permission denied, audio
    /// server restarted, …). The owner drops it and reopens later.
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioStreamInfo {
    /// Device / node actually opened.
    pub device: String,
    /// Native rate of the device (the API side is always 48 kHz).
    pub sample_rate: u32,
    pub channels: u16,
    /// Callback period in frames, if known.
    pub period_frames: u32,
    /// Device latency reported by the OS (ms), if known.
    pub device_latency_ms: f64,
}

/// A running audio stream (capture, playback or virtual device). Dropping
/// it stops the stream and, for virtual devices, removes the device.
pub trait AudioStream: Send {
    fn info(&self) -> AudioStreamInfo;
    /// Virtual devices: some application is connected right now (the node
    /// is being scheduled). Physical streams: always true while running.
    fn active(&self) -> bool {
        true
    }
    /// Largest gap between two callbacks since the previous call, ms.
    fn take_max_callback_gap_ms(&self) -> f64 {
        0.0
    }
}

// ---------------------------------------------------------------------------
// The backend trait
// ---------------------------------------------------------------------------

pub trait PlatformBackend: Send + Sync + 'static {
    /// Short platform name for logs/status ("linux-wayland", "macos", …).
    fn name(&self) -> String;

    // --- keystore ---------------------------------------------------------
    /// Read a secret from the OS keystore (Keychain / Secret Service).
    /// `Ok(None)` = keystore works but has no such entry.
    fn keystore_get(&self, _name: &str) -> anyhow::Result<Option<String>> {
        anyhow::bail!("no OS keystore")
    }
    fn keystore_set(&self, _name: &str, _secret: &str) -> anyhow::Result<()> {
        anyhow::bail!("no OS keystore")
    }
    /// Whether an OS keystore is usable right now.
    fn keystore_available(&self) -> bool {
        false
    }

    // --- clipboard --------------------------------------------------------
    /// Start watching the clipboard; changes are sent on `tx`.
    fn clipboard_watch(&self, _tx: mpsc::Sender<ClipboardChange>) -> Cap<()> {
        Err(Unsupported::new(
            "clipboard not implemented on this platform",
        ))
    }
    /// Replace the clipboard contents. May block briefly.
    fn clipboard_write(&self, _content: ClipboardContent) -> anyhow::Result<()> {
        anyhow::bail!("clipboard not implemented on this platform")
    }

    // --- notifications ----------------------------------------------------
    /// Show a native notification banner.
    fn show_notification(&self, n: &Notification) -> anyhow::Result<()>;
    /// Watch notifications posted by *other* apps (for mirroring).
    fn watch_notifications(&self, _tx: mpsc::Sender<Notification>) -> Cap<()> {
        Err(Unsupported::new(
            "notification capture not available on this platform",
        ))
    }

    // --- do not disturb ---------------------------------------------------
    fn get_dnd(&self) -> Cap<bool> {
        Err(Unsupported::new("DND not available"))
    }
    fn set_dnd(&self, _on: bool) -> anyhow::Result<()> {
        anyhow::bail!("DND not available")
    }
    /// Watch DND changes. If unsupported, DND is "set-only" on this device.
    fn watch_dnd(&self, _tx: mpsc::Sender<bool>) -> Cap<()> {
        Err(Unsupported::new("DND change detection not available"))
    }

    // --- power ------------------------------------------------------------
    fn watch_power(&self, _tx: mpsc::Sender<PowerEvent>) -> Cap<()> {
        Err(Unsupported::new("power events not available"))
    }

    // --- input ------------------------------------------------------------
    fn input_capture(&self, _tx: mpsc::Sender<LocalInput>) -> Cap<Box<dyn InputCapture>> {
        Err(Unsupported::new("input capture not available"))
    }
    fn input_injector(&self) -> Cap<Box<dyn InputInjector>> {
        Err(Unsupported::new("input injection not available"))
    }

    // --- camera -----------------------------------------------------------
    fn camera_source(
        &self,
        _params: CameraParams,
        _tx: mpsc::Sender<CameraEvent>,
    ) -> Cap<Box<dyn CameraSource>> {
        Err(Unsupported::new("no camera source on this platform"))
    }
    fn camera_sink(&self, _params: CameraParams) -> Cap<Box<dyn CameraSink>> {
        Err(Unsupported::new("no virtual camera sink on this platform"))
    }

    // --- audio ------------------------------------------------------------
    /// Capture from a physical input (macOS: the microphone).
    fn audio_capture(
        &self,
        _spec: AudioSpec,
        _sink: PcmSink,
        _ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        Err(Unsupported::new("no audio capture on this platform"))
    }
    /// Play to a physical output (macOS: the speakers / headphones).
    fn audio_playback(
        &self,
        _spec: AudioSpec,
        _src: PcmSource,
        _ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        Err(Unsupported::new("no audio playback on this platform"))
    }
    /// Create a virtual microphone other apps can record from (Ubuntu:
    /// PipeWire `Audio/Source` "Linker Microphone"), fed by `src`.
    fn virtual_mic(
        &self,
        _spec: AudioSpec,
        _src: PcmSource,
        _ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        Err(Unsupported::new("no virtual microphone on this platform"))
    }
    /// Create a virtual speaker other apps can play to (Ubuntu: PipeWire
    /// `Audio/Sink` "Linker Speaker"); what they play is delivered to `sink`.
    fn virtual_speaker(
        &self,
        _spec: AudioSpec,
        _sink: PcmSink,
        _ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        Err(Unsupported::new("no virtual speaker on this platform"))
    }
}

/// Wall clock in µs since UNIX epoch.
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Wall clock in ms since UNIX epoch.
pub fn now_ms() -> u64 {
    now_us() / 1000
}
