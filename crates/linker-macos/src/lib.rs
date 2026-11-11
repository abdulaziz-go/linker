//! macOS backend for Linker (100% userspace — no kernel extensions).
//!
//! | capability    | implementation                                                  |
//! |---------------|-----------------------------------------------------------------|
//! | clipboard     | `NSPasteboard.general`, `changeCount` polling (300 ms)          |
//! | notifications | show: `osascript`; capture: usernoted SQLite DB (best effort)   |
//! | DND / Focus   | read `~/Library/DoNotDisturb/DB/Assertions.json`;               |
//! |               | set via Shortcuts ("Linker DND On" / "Linker DND Off")          |
//! | power         | `NSWorkspace` will-sleep / did-wake notifications               |
//! | KVM           | `CGEventTap` capture, `CGEventPost` injection                   |
//! | camera source | AVFoundation capture → VideoToolbox H.264 (hardware, real-time) |
//! | audio capture | `cpal` / CoreAudio default (or named) input, after the TCC      |
//! |               | microphone check; device rate/channels → 48 kHz stereo `i16`    |
//! | audio playback| `cpal` / CoreAudio default (or named) output, ≈128-frame fixed  |
//! |               | buffer, 48 kHz stereo `i16` → device rate/channels              |
//! | keystore      | login Keychain (generic password)                               |

#![cfg(target_os = "macos")]

mod audio;
mod camera;
mod dnd;
mod ffi;
mod input;
mod keystore;
mod notify;
mod objc;
mod pasteboard;
mod power;

use linker_platform::*;
use tokio::sync::mpsc;

pub use power::run_with_main_loop;

pub struct MacBackend {
    pasteboard: std::sync::Arc<pasteboard::Pasteboard>,
}

impl MacBackend {
    pub fn new() -> MacBackend {
        MacBackend {
            pasteboard: std::sync::Arc::new(pasteboard::Pasteboard::new()),
        }
    }
}

impl Default for MacBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformBackend for MacBackend {
    fn name(&self) -> String {
        format!("macos-{}", objc::macos_version())
    }

    fn keystore_get(&self, name: &str) -> anyhow::Result<Option<String>> {
        keystore::get(name)
    }
    fn keystore_set(&self, name: &str, secret: &str) -> anyhow::Result<()> {
        keystore::set(name, secret)
    }
    fn keystore_available(&self) -> bool {
        true
    }

    fn clipboard_watch(&self, tx: mpsc::Sender<ClipboardChange>) -> Cap<()> {
        pasteboard::watch(self.pasteboard.clone(), tx)
    }
    fn clipboard_write(&self, content: ClipboardContent) -> anyhow::Result<()> {
        self.pasteboard.write(content)
    }

    fn show_notification(&self, n: &Notification) -> anyhow::Result<()> {
        notify::show(n)
    }
    fn watch_notifications(&self, tx: mpsc::Sender<Notification>) -> Cap<()> {
        notify::watch(tx)
    }

    fn get_dnd(&self) -> Cap<bool> {
        dnd::get().map_err(Unsupported::from)
    }
    fn set_dnd(&self, on: bool) -> anyhow::Result<()> {
        dnd::set(on)
    }
    fn watch_dnd(&self, tx: mpsc::Sender<bool>) -> Cap<()> {
        dnd::watch(tx)
    }

    fn watch_power(&self, tx: mpsc::Sender<PowerEvent>) -> Cap<()> {
        power::subscribe(tx)
    }

    fn input_capture(&self, tx: mpsc::Sender<LocalInput>) -> Cap<Box<dyn InputCapture>> {
        input::capture(tx)
    }
    fn input_injector(&self) -> Cap<Box<dyn InputInjector>> {
        input::injector()
    }
    fn input_injection_ready(&self) -> Cap<()> {
        input::injection_ready()
    }

    fn camera_source(
        &self,
        params: CameraParams,
        tx: mpsc::Sender<CameraEvent>,
    ) -> Cap<Box<dyn CameraSource>> {
        camera::start(params, tx)
    }

    fn audio_capture(
        &self,
        spec: AudioSpec,
        sink: PcmSink,
        ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        audio::capture(spec, sink, ev)
    }
    fn audio_playback(
        &self,
        spec: AudioSpec,
        src: PcmSource,
        ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        audio::playback(spec, src, ev)
    }
}
