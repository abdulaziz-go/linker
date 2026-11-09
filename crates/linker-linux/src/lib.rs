//! Linux (Ubuntu / GNOME) backend for Linker.
//!
//! | capability    | implementation                                                    |
//! |---------------|-------------------------------------------------------------------|
//! | clipboard     | `arboard` (X11 / Xwayland selections; `wlr-data-control` on       |
//! |               | wlroots/KDE Wayland), change detection via XFixes, else polling   |
//! | notifications | D-Bus `org.freedesktop.Notifications`                             |
//! | DND           | GSettings `org.gnome.desktop.notifications show-banners`          |
//! | power         | systemd-logind `PrepareForSleep` (system D-Bus)                   |
//! | KVM capture   | evdev (`/dev/input/event*`, `EVIOCGRAB` while captured)           |
//! | KVM inject    | kernel `uinput` virtual keyboard + pointer                        |
//! | camera sink   | H.264 decode (openh264) → `v4l2loopback` device "Linker Camera"   |
//! | keystore      | Secret Service via `secret-tool` (libsecret)                      |

#![cfg(target_os = "linux")]

mod camera;
mod clipboard;
mod dnd;
mod input;
mod keystore;
mod notify;
mod power;
mod session;

use std::sync::Arc;

use linker_platform::*;
use tokio::sync::mpsc;

pub use session::DisplayServer;

pub struct LinuxBackend {
    clipboard: Arc<clipboard::Clip>,
    notifier: notify::Notifier,
}

impl LinuxBackend {
    pub fn new() -> LinuxBackend {
        session::refresh_env();
        LinuxBackend {
            clipboard: Arc::new(clipboard::Clip::new()),
            notifier: notify::Notifier::new(),
        }
    }
}

impl Default for LinuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformBackend for LinuxBackend {
    fn name(&self) -> String {
        format!("linux-{}", session::display_server().as_str())
    }

    fn keystore_get(&self, name: &str) -> anyhow::Result<Option<String>> {
        keystore::get(name)
    }
    fn keystore_set(&self, name: &str, secret: &str) -> anyhow::Result<()> {
        keystore::set(name, secret)
    }
    fn keystore_available(&self) -> bool {
        keystore::available()
    }

    fn clipboard_watch(&self, tx: mpsc::Sender<ClipboardChange>) -> Cap<()> {
        clipboard::watch(self.clipboard.clone(), tx)
    }
    fn clipboard_write(&self, content: ClipboardContent) -> anyhow::Result<()> {
        self.clipboard.write(content)
    }

    fn show_notification(&self, n: &Notification) -> anyhow::Result<()> {
        self.notifier.show(n.clone());
        Ok(())
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
        power::watch(tx);
        Ok(())
    }

    fn input_capture(&self, tx: mpsc::Sender<LocalInput>) -> Cap<Box<dyn InputCapture>> {
        input::capture(tx)
    }
    fn input_injector(&self) -> Cap<Box<dyn InputInjector>> {
        input::injector()
    }

    fn camera_sink(&self, params: CameraParams) -> Cap<Box<dyn CameraSink>> {
        camera::open_sink(params)
    }
}
