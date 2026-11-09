//! Graphical-session environment.
//!
//! As a systemd *user* service started at boot (with linger), linkerd may
//! start before anyone logs in, without `DISPLAY` / `WAYLAND_DISPLAY`. GNOME
//! imports those into the systemd user manager at login, so we re-read them
//! from `systemctl --user show-environment` whenever a session-dependent
//! feature needs them.

use std::process::Command;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayServer {
    Wayland,
    X11,
    None,
}

impl DisplayServer {
    pub fn as_str(self) -> &'static str {
        match self {
            DisplayServer::Wayland => "wayland",
            DisplayServer::X11 => "x11",
            DisplayServer::None => "headless",
        }
    }
}

const VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
];

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Import graphical-session variables from the systemd user manager.
pub fn refresh_env() {
    let _g = ENV_LOCK.lock().expect("env lock");
    let Ok(out) = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((k, v)) = line.split_once('=') {
            if VARS.contains(&k) && std::env::var(k).ok().as_deref() != Some(v) {
                tracing::debug!(var = k, "imported session environment");
                // SAFETY-ish: edition 2021 `set_var`; serialized by ENV_LOCK and
                // only touched for these session variables.
                std::env::set_var(k, v);
            }
        }
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        // SAFETY: getuid has no failure modes.
        let uid = unsafe { libc::getuid() };
        std::env::set_var("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    }
}

pub fn display_server() -> DisplayServer {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if session == "wayland" || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        DisplayServer::Wayland
    } else if std::env::var_os("DISPLAY").is_some() {
        DisplayServer::X11
    } else {
        DisplayServer::None
    }
}

/// Logical screen size, from the X server (Xwayland mirrors the Wayland
/// desktop layout on GNOME). `None` if unknown.
pub fn screen_size() -> Option<(f64, f64)> {
    let (conn, screen) = x11rb::connect(None).ok()?;
    let s = &x11rb::connection::Connection::setup(&conn).roots[screen];
    Some((s.width_in_pixels as f64, s.height_in_pixels as f64))
}
