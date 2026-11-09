//! Software KVM: one keyboard + mouse across both screens.
//!
//! * **Server** (default: Ubuntu, owns the physical devices) captures local
//!   input. When the pointer pushes past `peer_edge`, it grabs the devices,
//!   sends `KvmEnter{edge, ratio, modifiers}`, and forwards every event.
//! * **Client** (default: Mac) injects what it receives. When its pointer
//!   pushes back out through the edge it entered from, it sends
//!   `KvmReturn{ratio}`; the server releases the grab and places its cursor.
//! * **Safety**: if the link drops while captured, the server ungrabs at once
//!   (dead-man's switch) and the client releases every held key/button. The
//!   panic hotkey (default Ctrl+Alt+Shift+Esc) force-releases capture.

use std::collections::HashSet;
use std::sync::Arc;

use linker_platform::{InputCapture, InputInjector, LocalInput};
use linker_proto::keycode::{self, is_modifier, modifier_bit};
use linker_proto::{
    modifiers, Edge, Feature, KeyEvent, KvmEnter, KvmLeave, KvmReturn, Message, MouseButton,
    MouseButtonEvent, MouseMove, Scroll,
};
use tokio::sync::mpsc;

use crate::config::KvmRole;
use crate::Shared;

/// Tracks the pointer and detects "pushing through" an edge.
#[derive(Debug, Clone)]
pub struct EdgeTracker {
    pub w: f64,
    pub h: f64,
    pub x: f64,
    pub y: f64,
    pub edge: Edge,
    pub threshold: f64,
    pressure: f64,
}

impl EdgeTracker {
    pub fn new(w: f64, h: f64, edge: Edge, threshold: f64) -> Self {
        EdgeTracker {
            w,
            h,
            x: w / 2.0,
            y: h / 2.0,
            edge,
            threshold,
            pressure: 0.0,
        }
    }

    /// Feed a motion. `abs` is the real position when known; otherwise a
    /// virtual cursor is integrated from the deltas and clamped to the screen.
    /// Returns `Some(ratio)` when the pointer pushed through `edge`.
    pub fn update(&mut self, dx: f64, dy: f64, abs: Option<(f64, f64)>) -> Option<f64> {
        let (nx, ny) = match abs {
            Some(p) => p,
            None => (self.x + dx, self.y + dy),
        };
        self.x = nx.clamp(0.0, self.w - 1.0);
        self.y = ny.clamp(0.0, self.h - 1.0);
        let (at_edge, outward) = match self.edge {
            Edge::Left => (self.x <= 0.5, -dx),
            Edge::Right => (self.x >= self.w - 1.5, dx),
            Edge::Top => (self.y <= 0.5, -dy),
            Edge::Bottom => (self.y >= self.h - 1.5, dy),
        };
        if at_edge && outward > 0.0 {
            self.pressure += outward;
        } else if !at_edge || outward < 0.0 {
            self.pressure = 0.0;
        }
        if self.pressure >= self.threshold {
            self.pressure = 0.0;
            Some(self.ratio())
        } else {
            None
        }
    }

    pub fn ratio(&self) -> f64 {
        match self.edge {
            Edge::Left | Edge::Right => (self.y / self.h).clamp(0.0, 1.0),
            Edge::Top | Edge::Bottom => (self.x / self.w).clamp(0.0, 1.0),
        }
    }

    /// Put the tracker on `edge` at `ratio`, just inside the screen.
    pub fn place(&mut self, edge: Edge, ratio: f64) {
        let r = ratio.clamp(0.0, 1.0);
        match edge {
            Edge::Left => (self.x, self.y) = (2.0, r * self.h),
            Edge::Right => (self.x, self.y) = (self.w - 3.0, r * self.h),
            Edge::Top => (self.x, self.y) = (r * self.w, 2.0),
            Edge::Bottom => (self.x, self.y) = (r * self.w, self.h - 3.0),
        }
        self.pressure = 0.0;
    }
}

pub async fn run(shared: Arc<Shared>, rx: mpsc::Receiver<Message>) {
    if !shared.cfg.kvm.enabled {
        shared.status.feature("kvm", "disabled");
        super::drain(rx);
        return;
    }
    match shared.cfg.kvm_role() {
        KvmRole::Client => run_client(shared, rx).await,
        _ => run_server(shared, rx).await,
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

struct Server {
    capture: Box<dyn InputCapture>,
    tracker: EdgeTracker,
    remote: bool,
    held: HashSet<u16>,
    panic: Vec<u16>,
}

impl Server {
    fn modifiers(&self) -> u8 {
        self.held
            .iter()
            .filter(|k| is_modifier(**k))
            .fold(0, |m, k| m | modifier_bit(*k))
    }

    fn enter(&mut self, shared: &Shared, ratio: f64) {
        let Some(link) = shared.link() else { return };
        if !link.peer_has(Feature::KvmClient) {
            return;
        }
        if let Err(e) = self.capture.set_captured(true) {
            tracing::warn!("could not grab input: {e:#}");
            return;
        }
        self.remote = true;
        let edge = self.tracker.edge.opposite();
        link.send(Message::KvmEnter(KvmEnter {
            edge,
            ratio: ratio as f32,
            modifiers: self.modifiers(),
        }));
        shared.status.feature(
            "kvm",
            format!("server: controlling {}", link.peer.device_name),
        );
        tracing::debug!(ratio, "KVM → remote");
    }

    fn release(&mut self, shared: &Shared, ratio: Option<f64>, why: &str) {
        if !self.remote {
            return;
        }
        self.remote = false;
        if let Err(e) = self.capture.set_captured(false) {
            tracing::error!("failed to release input grab: {e:#}");
        }
        if let Some(link) = shared.link() {
            link.send(Message::KvmLeave(KvmLeave));
        }
        let ratio = ratio.unwrap_or_else(|| self.tracker.ratio());
        let edge = self.tracker.edge;
        self.tracker.place(edge, ratio);
        if let Err(e) = self.capture.warp_to(edge, ratio) {
            tracing::debug!("warp failed: {e:#}");
        }
        shared.status.feature("kvm", "server: local");
        tracing::debug!(why, "KVM → local");
    }
}

async fn run_server(shared: Arc<Shared>, mut rx: mpsc::Receiver<Message>) {
    let cfg = shared.cfg.kvm.clone();
    let (tx, mut input) = mpsc::channel::<LocalInput>(4096);
    let capture = match shared.backend.input_capture(tx) {
        Ok(c) => c,
        Err(e) => {
            shared
                .status
                .feature("kvm", format!("unsupported ({})", e.0));
            super::drain(rx);
            return;
        }
    };
    let (sw, sh) = capture.screen_size();
    let (w, h) = (
        cfg.screen_width.unwrap_or(sw),
        cfg.screen_height.unwrap_or(sh),
    );
    let mut s = Server {
        capture,
        tracker: EdgeTracker::new(w, h, cfg.peer_edge.into(), cfg.edge_threshold),
        remote: false,
        held: HashSet::new(),
        panic: cfg.panic_hotkey.clone(),
    };
    shared.status.feature(
        "kvm",
        format!(
            "server: local (screen {w:.0}x{h:.0}, peer on {:?} edge)",
            s.tracker.edge
        ),
    );
    let mut link_rx = shared.watch_link();

    loop {
        tokio::select! {
            changed = link_rx.changed() => {
                if changed.is_err() { return; }
                let up = link_rx.borrow_and_update().is_some();
                // Dead-man's switch: never stay grabbed without a link.
                if !up {
                    s.release(&shared, None, "link lost");
                } else if s.remote {
                    s.release(&shared, None, "new session");
                }
            }

            Some(ev) = input.recv() => {
                match ev {
                    LocalInput::Motion { dx, dy, pos } => {
                        if s.remote {
                            if let Some(l) = shared.link() { l.send(Message::MouseMove(MouseMove { dx, dy })); }
                        } else if let Some(ratio) = s.tracker.update(dx as f64, dy as f64, pos) {
                            s.enter(&shared, ratio);
                        }
                    }
                    LocalInput::Button { button, pressed } => if s.remote {
                        if let Some(l) = shared.link() { l.send(Message::MouseButton(MouseButtonEvent { button, pressed })); }
                    },
                    LocalInput::Scroll { dx, dy } => if s.remote {
                        if let Some(l) = shared.link() { l.send(Message::Scroll(Scroll { dx, dy })); }
                    },
                    LocalInput::Key { hid, pressed } => {
                        if pressed { s.held.insert(hid); } else { s.held.remove(&hid); }
                        if s.remote {
                            if !s.panic.is_empty() && s.panic.iter().all(|k| s.held.contains(k)) {
                                tracing::warn!("panic hotkey: releasing KVM capture");
                                s.release(&shared, None, "panic hotkey");
                                continue;
                            }
                            if let Some(l) = shared.link() { l.send(Message::Key(KeyEvent { hid, pressed })); }
                        }
                    }
                }
            }

            Some(msg) = rx.recv() => {
                if let Message::KvmReturn(KvmReturn { ratio }) = msg {
                    s.release(&shared, Some(ratio as f64), "client returned control");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

struct Client {
    inj: Box<dyn InputInjector>,
    tracker: EdgeTracker,
    active: bool,
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
    swap: bool,
}

impl Client {
    fn release_all(&mut self) {
        for k in std::mem::take(&mut self.keys) {
            let _ = self.inj.key(k, false);
        }
        for b in std::mem::take(&mut self.buttons) {
            let _ = self.inj.button(b, false);
        }
    }

    fn map_key(&self, hid: u16) -> u16 {
        if self.swap {
            keycode::swap_ctrl_meta(hid)
        } else {
            hid
        }
    }

    fn key(&mut self, hid: u16, pressed: bool) {
        let hid = self.map_key(hid);
        if pressed {
            self.keys.insert(hid);
        } else {
            self.keys.remove(&hid);
        }
        if let Err(e) = self.inj.key(hid, pressed) {
            tracing::debug!("inject key failed: {e:#}");
        }
    }
}

async fn run_client(shared: Arc<Shared>, mut rx: mpsc::Receiver<Message>) {
    let cfg = shared.cfg.kvm.clone();
    let inj = match shared.backend.input_injector() {
        Ok(i) => i,
        Err(e) => {
            shared
                .status
                .feature("kvm", format!("unsupported ({})", e.0));
            super::drain(rx);
            return;
        }
    };
    let (w, h) = inj.screen_size();
    let swap = cfg.swap_ctrl_cmd && cfg!(target_os = "macos");
    let mut c = Client {
        inj,
        tracker: EdgeTracker::new(w, h, Edge::Left, cfg.edge_threshold),
        active: false,
        keys: HashSet::new(),
        buttons: HashSet::new(),
        swap,
    };
    shared
        .status
        .feature("kvm", format!("client: idle (screen {w:.0}x{h:.0})"));
    let mut link_rx = shared.watch_link();

    loop {
        tokio::select! {
            changed = link_rx.changed() => {
                if changed.is_err() { return; }
                link_rx.borrow_and_update();
                if c.active {
                    c.release_all();
                    c.active = false;
                    shared.status.feature("kvm", "client: idle");
                }
            }
            Some(msg) = rx.recv() => match msg {
                Message::KvmEnter(KvmEnter { edge, ratio, modifiers: mods }) => {
                    c.release_all();
                    c.tracker.edge = edge;
                    c.tracker.place(edge, ratio as f64);
                    if let Err(e) = c.inj.warp_to(edge, ratio as f64) {
                        tracing::debug!("warp failed: {e:#}");
                    }
                    // Re-sync modifiers held on the server at handoff time.
                    for (bit, hid) in [
                        (modifiers::SHIFT, keycode::HID_LSHIFT),
                        (modifiers::CTRL, keycode::HID_LCTRL),
                        (modifiers::ALT, keycode::HID_LALT),
                        (modifiers::META, keycode::HID_LMETA),
                    ] {
                        if mods & bit != 0 {
                            c.key(hid, true);
                        }
                    }
                    c.active = true;
                    shared.status.feature("kvm", "client: active (controlled by peer)");
                }
                Message::KvmLeave(_) => {
                    c.release_all();
                    c.active = false;
                    shared.status.feature("kvm", "client: idle");
                }
                Message::MouseMove(MouseMove { dx, dy }) if c.active => {
                    if let Err(e) = c.inj.move_relative(dx, dy) {
                        tracing::debug!("inject move failed: {e:#}");
                    }
                    let pos = c.inj.cursor_position();
                    if let Some(ratio) = c.tracker.update(dx as f64, dy as f64, pos) {
                        c.release_all();
                        c.active = false;
                        if let Some(l) = shared.link() {
                            l.send(Message::KvmReturn(KvmReturn { ratio: ratio as f32 }));
                        }
                        shared.status.feature("kvm", "client: idle");
                    }
                }
                Message::MouseButton(MouseButtonEvent { button, pressed }) if c.active => {
                    if pressed { c.buttons.insert(button); } else { c.buttons.remove(&button); }
                    let _ = c.inj.button(button, pressed);
                }
                Message::Scroll(Scroll { dx, dy }) if c.active => { let _ = c.inj.scroll(dx, dy); }
                Message::Key(KeyEvent { hid, pressed }) if c.active => c.key(hid, pressed),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_right_edge_with_virtual_cursor() {
        let mut t = EdgeTracker::new(1920.0, 1080.0, Edge::Right, 2.0);
        // Move hard right: clamps at the edge, then pressure builds.
        assert_eq!(t.update(5000.0, 0.0, None), Some(0.5));
    }

    #[test]
    fn moving_along_edge_does_not_cross() {
        let mut t = EdgeTracker::new(1000.0, 1000.0, Edge::Right, 50.0);
        t.place(Edge::Right, 0.5);
        t.x = 999.0;
        assert_eq!(t.update(0.0, 10.0, None), None);
        assert_eq!(t.update(0.0, -10.0, None), None);
    }

    #[test]
    fn pressure_needs_threshold_with_absolute_positions() {
        let mut t = EdgeTracker::new(1440.0, 900.0, Edge::Left, 10.0);
        assert_eq!(t.update(-4.0, 0.0, Some((0.0, 450.0))), None);
        assert_eq!(t.update(-4.0, 0.0, Some((0.0, 450.0))), None);
        assert_eq!(t.update(-4.0, 0.0, Some((0.0, 450.0))), Some(0.5));
    }

    #[test]
    fn moving_away_resets_pressure() {
        let mut t = EdgeTracker::new(100.0, 100.0, Edge::Top, 10.0);
        assert_eq!(t.update(0.0, -6.0, Some((50.0, 0.0))), None);
        assert_eq!(t.update(0.0, 3.0, Some((50.0, 3.0))), None);
        assert_eq!(t.update(0.0, -6.0, Some((50.0, 0.0))), None);
    }

    #[test]
    fn place_is_inside_screen() {
        let mut t = EdgeTracker::new(800.0, 600.0, Edge::Bottom, 1.0);
        t.place(Edge::Bottom, 0.25);
        assert_eq!((t.x, t.y), (200.0, 597.0));
        assert!((t.ratio() - 0.25).abs() < 1e-9);
    }
}
