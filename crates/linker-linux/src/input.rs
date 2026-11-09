//! KVM on Linux: capture via evdev (`/dev/input/event*`), suppression via
//! `EVIOCGRAB`, injection via the kernel `uinput` module.
//!
//! Works identically on Wayland and X11 because it sits below the display
//! server. Requirements: membership in the `input` group (read event
//! devices) and write access to `/dev/uinput` (udev rule in the runbook).
//!
//! Limitation: evdev sees *relative* devices (mice). Touchpads report
//! absolute multitouch data that only libinput turns into pointer motion, so
//! a touchpad can't drive the edge handoff on the server.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode, RelativeAxisCode};
use linker_platform::{
    Cap, Edge, InputCapture, InputInjector, LocalInput, MouseButton, Unsupported,
};
use linker_proto::keycode::{all_evdev_codes, evdev_from_hid, hid_from_evdev};
use tokio::sync::{mpsc, watch};

pub const VIRTUAL_NAME: &str = "Linker Virtual Input";

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

fn button_from_code(code: u16) -> Option<MouseButton> {
    Some(match code {
        BTN_LEFT => MouseButton::Left,
        BTN_RIGHT => MouseButton::Right,
        BTN_MIDDLE => MouseButton::Middle,
        BTN_SIDE => MouseButton::Back,
        BTN_EXTRA => MouseButton::Forward,
        _ => return None,
    })
}

fn code_from_button(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
    }
}

fn is_relevant(dev: &Device) -> bool {
    if dev.name() == Some(VIRTUAL_NAME) {
        return false;
    }
    let keyboard = dev
        .supported_keys()
        .is_some_and(|k| k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_ENTER));
    let mouse = dev
        .supported_relative_axes()
        .is_some_and(|r| r.contains(RelativeAxisCode::REL_X));
    keyboard || mouse
}

fn screen() -> (f64, f64) {
    crate::session::refresh_env();
    crate::session::screen_size().unwrap_or((1920.0, 1080.0))
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

struct LinuxCapture {
    grab: watch::Sender<bool>,
    pointer: Option<Pointer>,
    size: (f64, f64),
}

pub fn capture(tx: mpsc::Sender<LocalInput>) -> Cap<Box<dyn InputCapture>> {
    let found: Vec<PathBuf> = evdev::enumerate()
        .filter(|(_, d)| is_relevant(d))
        .map(|(p, _)| p)
        .collect();
    if found.is_empty() {
        let any = std::fs::read_dir("/dev/input")
            .map(|d| d.count())
            .unwrap_or(0);
        return Err(Unsupported::new(if any > 0 {
            "cannot read /dev/input/event* — add your user to the 'input' group and log in again"
        } else {
            "no input devices"
        }));
    }
    let (grab, grab_rx) = watch::channel(false);
    tokio::spawn(device_manager(tx, grab_rx));
    let pointer = Pointer::new()
        .map_err(|e| tracing::warn!("cursor warp disabled: {e:#}"))
        .ok();
    Ok(Box::new(LinuxCapture {
        grab,
        pointer,
        size: screen(),
    }))
}

/// Watches for (hot-plugged) devices and runs one reader per device.
async fn device_manager(tx: mpsc::Sender<LocalInput>, grab_rx: watch::Receiver<bool>) {
    let mut running: HashSet<PathBuf> = HashSet::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<PathBuf>();
    loop {
        while let Ok(p) = done_rx.try_recv() {
            running.remove(&p);
        }
        for (path, dev) in evdev::enumerate() {
            if running.contains(&path) || !is_relevant(&dev) {
                continue;
            }
            tracing::info!(device = dev.name().unwrap_or("?"), path = %path.display(), "KVM: capturing input device");
            running.insert(path.clone());
            let (tx, grab_rx, done_tx) = (tx.clone(), grab_rx.clone(), done_tx.clone());
            tokio::spawn(async move {
                if let Err(e) = read_device(dev, tx, grab_rx).await {
                    tracing::debug!(path = %path.display(), "input device closed: {e:#}");
                }
                let _ = done_tx.send(path);
            });
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn read_device(
    dev: Device,
    tx: mpsc::Sender<LocalInput>,
    mut grab_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut stream = dev.into_event_stream()?;
    let mut grabbed = false;
    let (mut dx, mut dy) = (0i32, 0i32);
    let apply_grab = |stream: &mut evdev::EventStream, want: bool, grabbed: &mut bool| {
        if want != *grabbed {
            let r = if want {
                stream.device_mut().grab()
            } else {
                stream.device_mut().ungrab()
            };
            match r {
                Ok(()) => *grabbed = want,
                Err(e) => tracing::warn!("EVIOCGRAB({want}) failed: {e}"),
            }
        }
    };
    let want = *grab_rx.borrow_and_update();
    apply_grab(&mut stream, want, &mut grabbed);
    loop {
        tokio::select! {
            changed = grab_rx.changed() => {
                if changed.is_err() {
                    apply_grab(&mut stream, false, &mut grabbed);
                    return Ok(());
                }
                let want = *grab_rx.borrow_and_update();
                apply_grab(&mut stream, want, &mut grabbed);
            }
            ev = stream.next_event() => {
                let ev = ev?;
                let out = match ev.event_type() {
                    EventType::RELATIVE => {
                        match RelativeAxisCode(ev.code()) {
                            RelativeAxisCode::REL_X => dx += ev.value(),
                            RelativeAxisCode::REL_Y => dy += ev.value(),
                            RelativeAxisCode::REL_WHEEL => { tx.send(LocalInput::Scroll { dx: 0, dy: ev.value() }).await?; }
                            RelativeAxisCode::REL_HWHEEL => { tx.send(LocalInput::Scroll { dx: ev.value(), dy: 0 }).await?; }
                            _ => {}
                        }
                        None
                    }
                    EventType::SYNCHRONIZATION => {
                        if dx != 0 || dy != 0 {
                            let m = LocalInput::Motion { dx, dy, pos: None };
                            (dx, dy) = (0, 0);
                            Some(m)
                        } else {
                            None
                        }
                    }
                    EventType::KEY if ev.value() != 2 => {
                        let pressed = ev.value() == 1;
                        match button_from_code(ev.code()) {
                            Some(button) => Some(LocalInput::Button { button, pressed }),
                            None => hid_from_evdev(ev.code()).map(|hid| LocalInput::Key { hid, pressed }),
                        }
                    }
                    _ => None,
                };
                if let Some(o) = out {
                    tx.send(o).await?;
                }
            }
        }
    }
}

impl InputCapture for LinuxCapture {
    fn set_captured(&mut self, captured: bool) -> anyhow::Result<()> {
        self.grab.send_replace(captured);
        Ok(())
    }

    fn screen_size(&self) -> (f64, f64) {
        self.size
    }

    fn warp_to(&mut self, edge: Edge, ratio: f64) -> anyhow::Result<()> {
        if crate::session::display_server() == crate::session::DisplayServer::X11 {
            return x11_warp(edge, ratio, self.size);
        }
        // Wayland: no absolute warp for clients; pin the pointer to the edge
        // with a large relative move through uinput (y is left unchanged).
        let p = self.pointer.as_mut().context("no uinput pointer")?;
        p.pin(edge, self.size)
    }
}

fn x11_warp(edge: Edge, ratio: f64, (w, h): (f64, f64)) -> anyhow::Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let (x, y) = match edge {
        Edge::Left => (2.0, ratio * h),
        Edge::Right => (w - 3.0, ratio * h),
        Edge::Top => (ratio * w, 2.0),
        Edge::Bottom => (ratio * w, h - 3.0),
    };
    conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, x as i16, y as i16)?;
    conn.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// uinput
// ---------------------------------------------------------------------------

fn uinput_error(e: std::io::Error) -> Unsupported {
    Unsupported::new(format!(
        "cannot open /dev/uinput ({e}) — load the uinput module and install the udev rule from the runbook"
    ))
}

struct Pointer(evdev::uinput::VirtualDevice);

impl Pointer {
    fn new() -> anyhow::Result<Pointer> {
        Ok(Pointer(build_device(false)?))
    }

    fn pin(&mut self, edge: Edge, (w, h): (f64, f64)) -> anyhow::Result<()> {
        let (dx, dy) = match edge {
            Edge::Left => (-(w as i32) * 2, 0),
            Edge::Right => ((w as i32) * 2, 0),
            Edge::Top => (0, -(h as i32) * 2),
            Edge::Bottom => (0, (h as i32) * 2),
        };
        emit_rel(&mut self.0, dx, dy)
    }
}

fn build_device(keyboard: bool) -> anyhow::Result<evdev::uinput::VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
        keys.insert(KeyCode(b));
    }
    if keyboard {
        for c in all_evdev_codes() {
            keys.insert(KeyCode(c));
        }
    }
    let mut rel = AttributeSet::<RelativeAxisCode>::new();
    for a in [
        RelativeAxisCode::REL_X,
        RelativeAxisCode::REL_Y,
        RelativeAxisCode::REL_WHEEL,
        RelativeAxisCode::REL_HWHEEL,
    ] {
        rel.insert(a);
    }
    Ok(evdev::uinput::VirtualDevice::builder()?
        .name(VIRTUAL_NAME)
        .with_keys(&keys)?
        .with_relative_axes(&rel)?
        .build()?)
}

fn emit_rel(dev: &mut evdev::uinput::VirtualDevice, dx: i32, dy: i32) -> anyhow::Result<()> {
    let mut evs = Vec::with_capacity(2);
    if dx != 0 {
        evs.push(InputEvent::new(
            EventType::RELATIVE.0,
            RelativeAxisCode::REL_X.0,
            dx,
        ));
    }
    if dy != 0 {
        evs.push(InputEvent::new(
            EventType::RELATIVE.0,
            RelativeAxisCode::REL_Y.0,
            dy,
        ));
    }
    if !evs.is_empty() {
        dev.emit(&evs)?;
    }
    Ok(())
}

struct LinuxInjector {
    dev: evdev::uinput::VirtualDevice,
    size: (f64, f64),
}

pub fn injector() -> Cap<Box<dyn InputInjector>> {
    let dev = build_device(true).map_err(|e| match e.downcast::<std::io::Error>() {
        Ok(io) => uinput_error(io),
        Err(e) => Unsupported::from(e),
    })?;
    Ok(Box::new(LinuxInjector {
        dev,
        size: screen(),
    }))
}

impl InputInjector for LinuxInjector {
    fn move_relative(&mut self, dx: i32, dy: i32) -> anyhow::Result<()> {
        emit_rel(&mut self.dev, dx, dy)
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> anyhow::Result<()> {
        self.dev.emit(&[InputEvent::new(
            EventType::KEY.0,
            code_from_button(button),
            pressed as i32,
        )])?;
        Ok(())
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> anyhow::Result<()> {
        let mut evs = Vec::new();
        if dy != 0 {
            evs.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL.0,
                dy,
            ));
        }
        if dx != 0 {
            evs.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_HWHEEL.0,
                dx,
            ));
        }
        if !evs.is_empty() {
            self.dev.emit(&evs)?;
        }
        Ok(())
    }

    fn key(&mut self, hid: u16, pressed: bool) -> anyhow::Result<()> {
        let code = evdev_from_hid(hid).with_context(|| format!("no evdev key for HID {hid:#x}"))?;
        self.dev
            .emit(&[InputEvent::new(EventType::KEY.0, code, pressed as i32)])?;
        Ok(())
    }

    fn warp_to(&mut self, edge: Edge, ratio: f64) -> anyhow::Result<()> {
        if crate::session::display_server() == crate::session::DisplayServer::X11 {
            return x11_warp(edge, ratio, self.size);
        }
        let (w, h) = self.size;
        let (dx, dy) = match edge {
            Edge::Left => (-(w as i32) * 2, 0),
            Edge::Right => ((w as i32) * 2, 0),
            Edge::Top => (0, -(h as i32) * 2),
            Edge::Bottom => (0, (h as i32) * 2),
        };
        emit_rel(&mut self.dev, dx, dy)
    }

    fn cursor_position(&self) -> Option<(f64, f64)> {
        None
    }

    fn screen_size(&self) -> (f64, f64) {
        self.size
    }
}
