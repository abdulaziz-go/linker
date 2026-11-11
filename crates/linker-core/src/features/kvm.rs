//! Software KVM: one keyboard + mouse across both screens.
//!
//! * **Server** (default: Ubuntu, owns the physical devices) captures local
//!   input. When the pointer pushes past `peer_edge` it offers control to the
//!   client with `KvmEnter`, and *only grabs the devices once the client
//!   acknowledges it is injecting* (`KvmAck`). It then forwards every event.
//! * **Client** (default: Mac) injects what it receives. It advertises
//!   `KvmReady{ready}` whenever its ability to inject changes (macOS:
//!   Accessibility trusted), acks each handoff, sends an "injecting OK"
//!   heartbeat while active, and reports injection errors at once. When its
//!   pointer pushes back out through the entry edge it sends `KvmReturn`.
//!
//! ## Fail-safe guarantees (the user must never be locked out)
//!
//! Capture is wrapped in an RAII [`CaptureGuard`] that ungrabs on *every* exit
//! path — normal return, task error/panic, shutdown. The server releases the
//! grab on any abnormal condition: no ack within [`ACK_TIMEOUT`], missing
//! heartbeats ([`LIVENESS_TIMEOUT`]), a reported injection error, the peer
//! going not-ready, the link dropping, the panic hotkey, or daemon shutdown.
//! The return path does not depend on the client: the server tracks the
//! virtual cursor itself and releases when it crosses back over the edge even
//! if the client is silent. Every forced release shows a desktop notification.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use linker_platform::{now_ms, InputCapture, InputInjector, LocalInput, Notification};
use linker_proto::keycode::{self, is_modifier, modifier_bit};
use linker_proto::{
    modifiers, Edge, Feature, KeyEvent, KvmAck, KvmEnter, KvmError, KvmHeartbeat, KvmLeave,
    KvmReady, KvmReturn, Message, MouseButton, MouseButtonEvent, MouseMove, Scroll,
};
use tokio::sync::mpsc;

use crate::config::KvmRole;
use crate::Shared;

/// After `KvmEnter`, the server waits this long for the client's `KvmAck`
/// before aborting the handoff and keeping input local.
const ACK_TIMEOUT: Duration = Duration::from_millis(400);
/// While control is on the client, the server releases the grab if no
/// heartbeat (or any other traffic) arrives for this long.
const LIVENESS_TIMEOUT: Duration = Duration::from_millis(1000);
/// How often the client sends an "injecting OK" heartbeat while active.
const CLIENT_HEARTBEAT: Duration = Duration::from_millis(250);
/// How often the client re-checks whether it can inject (detects the user
/// granting/revoking Accessibility while the daemon runs).
const READY_REPROBE: Duration = Duration::from_secs(2);
/// Server housekeeping tick (ack timeout + liveness checks).
const SERVER_TICK: Duration = Duration::from_millis(200);

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

/// Build Linker's own desktop notification (shown on every forced release).
fn notify(shared: &Shared, body: &str) {
    let n = Notification {
        app: "Linker".into(),
        title: "KVM released".into(),
        subtitle: String::new(),
        body: body.into(),
        timestamp_ms: now_ms(),
    };
    if let Err(e) = shared.backend.show_notification(&n) {
        tracing::warn!("could not show KVM notification: {e:#}");
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
// RAII grab guard: ungrabs on every exit path (drop, panic, shutdown).
// ---------------------------------------------------------------------------

struct CaptureGuard {
    cap: Box<dyn InputCapture>,
    engaged: bool,
}

impl CaptureGuard {
    fn new(cap: Box<dyn InputCapture>) -> Self {
        CaptureGuard {
            cap,
            engaged: false,
        }
    }
    fn engage(&mut self) -> anyhow::Result<()> {
        if !self.engaged {
            self.cap.set_captured(true)?;
            self.engaged = true;
        }
        Ok(())
    }
    fn release(&mut self) {
        if self.engaged {
            if let Err(e) = self.cap.set_captured(false) {
                tracing::error!("failed to release input grab: {e:#}");
            }
            self.engaged = false;
        }
    }
    fn warp_to(&mut self, edge: Edge, ratio: f64) -> anyhow::Result<()> {
        self.cap.warp_to(edge, ratio)
    }
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        // Last line of defence: no exit path may leave the devices grabbed.
        self.release();
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Everything the server state machine needs from its surroundings: sending
/// control messages to the client, inspecting the peer, and surfacing status /
/// notifications. Abstracted so the state machine can be unit-tested against a
/// fake client with no real link. [`LiveEnv`] is the production wiring.
trait ServerEnv {
    /// Send a control message to the client (a no-op if the link is down).
    fn send(&self, msg: Message);
    /// Is a client peer connected and advertising the client role right now?
    fn peer_is_client(&self) -> bool;
    fn peer_name(&self) -> String;
    fn set_status(&self, health: String);
    /// Show a desktop notification (used on every forced release).
    fn notify(&self, body: String);
}

struct LiveEnv<'a>(&'a Shared);

impl ServerEnv for LiveEnv<'_> {
    fn send(&self, msg: Message) {
        if let Some(l) = self.0.link() {
            l.send(msg);
        }
    }
    fn peer_is_client(&self) -> bool {
        self.0.link().is_some_and(|l| l.peer_has(Feature::KvmClient))
    }
    fn peer_name(&self) -> String {
        self.0
            .link()
            .map(|l| l.peer.device_name.clone())
            .unwrap_or_else(|| "peer".into())
    }
    fn set_status(&self, health: String) {
        self.0.status.feature("kvm", health);
    }
    fn notify(&self, body: String) {
        notify(self.0, &body);
    }
}

/// Where control currently is, from the server's point of view.
enum Phase {
    /// Input is local; watching for an edge crossing.
    Local,
    /// `KvmEnter` sent, waiting for the client's ack. Devices are NOT grabbed.
    Pending { since: Instant, ratio: f64 },
    /// Acked and grabbed; forwarding events. `last_alive` is the last time we
    /// heard an ack/heartbeat/return from the client.
    Active { last_alive: Instant },
}

struct Server {
    guard: CaptureGuard,
    /// Local-screen tracker (detects the outward crossing that starts a handoff).
    tracker: EdgeTracker,
    /// Client-screen tracker (server-side return detection, client-independent).
    ret: Option<EdgeTracker>,
    phase: Phase,
    held: HashSet<u16>,
    panic: Vec<u16>,
    /// The client has affirmatively advertised it can inject right now.
    peer_ready: bool,
    peer_edge: Edge,
    size: (f64, f64),
}

impl Server {
    fn modifiers(&self) -> u8 {
        self.held
            .iter()
            .filter(|k| is_modifier(**k))
            .fold(0, |m, k| m | modifier_bit(*k))
    }

    /// Role-aware panic detection: a configured left modifier matches the left
    /// *or* right variant, so Ctrl+Alt+Shift+Esc fires whichever hand is used.
    fn panic_triggered(&self) -> bool {
        if self.panic.is_empty() {
            return false;
        }
        self.panic.iter().all(|&k| {
            if is_modifier(k) {
                let bit = modifier_bit(k);
                self.held
                    .iter()
                    .any(|&h| is_modifier(h) && modifier_bit(h) == bit)
            } else {
                self.held.contains(&k)
            }
        })
    }

    /// Pointer crossed the edge: offer control to the client. Does not grab.
    fn begin_handoff(&mut self, env: &dyn ServerEnv, ratio: f64) {
        if !matches!(self.phase, Phase::Local) {
            return;
        }
        // Never hand off unless the peer is a client AND it says it can inject.
        if !env.peer_is_client() || !self.peer_ready {
            return;
        }
        env.send(Message::KvmEnter(KvmEnter {
            edge: self.peer_edge.opposite(),
            ratio: ratio as f32,
            modifiers: self.modifiers(),
        }));
        self.phase = Phase::Pending {
            since: Instant::now(),
            ratio,
        };
        env.set_status("server: handing off (waiting for Mac to accept)…".into());
        tracing::debug!(ratio, "KVM → offering control");
    }

    /// Client acknowledged the handoff: grab the devices and start forwarding.
    fn on_ack(&mut self, env: &dyn ServerEnv) {
        let Phase::Pending { ratio, .. } = self.phase else {
            return;
        };
        if let Err(e) = self.guard.engage() {
            tracing::error!("could not grab input after ack: {e:#}");
            self.phase = Phase::Local;
            self.peer_ready = false;
            env.set_status("server: local (grab failed)".into());
            env.notify("could not grab the local keyboard/mouse; staying local".into());
            return;
        }
        let client_edge = self.peer_edge.opposite();
        let mut ret = EdgeTracker::new(self.size.0, self.size.1, client_edge, self.tracker.threshold);
        ret.place(client_edge, ratio);
        self.ret = Some(ret);
        self.phase = Phase::Active {
            last_alive: Instant::now(),
        };
        env.set_status(format!("server: controlling {}", env.peer_name()));
        tracing::debug!("KVM → remote (acked)");
    }

    /// Return input to this machine. `notify_body` shows a banner when set
    /// (used for every forced/abnormal release).
    fn go_local(
        &mut self,
        env: &dyn ServerEnv,
        ratio: Option<f64>,
        status: &str,
        notify_body: Option<String>,
    ) {
        if matches!(self.phase, Phase::Local) {
            return;
        }
        let was_active = matches!(self.phase, Phase::Active { .. });
        self.phase = Phase::Local;
        self.ret = None;
        if was_active {
            self.guard.release();
            let ratio = ratio.unwrap_or_else(|| self.tracker.ratio());
            env.send(Message::KvmLeave(KvmLeave));
            let edge = self.peer_edge;
            self.tracker.place(edge, ratio);
            if let Err(e) = self.guard.warp_to(edge, ratio) {
                tracing::debug!("warp failed: {e:#}");
            }
        } else {
            // Aborting a pending handoff: the client may have warped/activated
            // on KvmEnter, so tell it to park and release any held keys.
            env.send(Message::KvmLeave(KvmLeave));
        }
        env.set_status(status.into());
        tracing::info!(status, "KVM → local");
        if let Some(body) = notify_body {
            env.notify(body);
        }
    }

    /// Periodic timer: abort un-acked handoffs; release on heartbeat loss.
    fn tick(&mut self, env: &dyn ServerEnv) {
        match self.phase {
            Phase::Pending { since, .. } if since.elapsed() > ACK_TIMEOUT => {
                self.peer_ready = false; // don't immediately retry and re-freeze
                self.go_local(
                    env,
                    None,
                    "server: local (Mac did not accept)",
                    Some("Mac is not accepting input (Accessibility permission missing?)".into()),
                );
            }
            Phase::Active { last_alive } if last_alive.elapsed() > LIVENESS_TIMEOUT => {
                self.go_local(
                    env,
                    None,
                    "server: local (Mac stopped responding)",
                    Some("Mac stopped responding while in control".into()),
                );
            }
            _ => {}
        }
    }

    /// A local physical event (from the grabbed/ungrabbed evdev stream).
    fn on_local_input(&mut self, env: &dyn ServerEnv, ev: LocalInput) {
        match ev {
            LocalInput::Motion { dx, dy, pos } => match self.phase {
                Phase::Active { .. } => {
                    env.send(Message::MouseMove(MouseMove { dx, dy }));
                    // Client-independent return detection: the server tracks the
                    // virtual cursor itself, so moving back across the edge
                    // returns control even if the client never sends KvmReturn.
                    let returned = self
                        .ret
                        .as_mut()
                        .and_then(|r| r.update(dx as f64, dy as f64, None));
                    if let Some(ratio) = returned {
                        self.go_local(env, Some(ratio), "server: local", None);
                    }
                }
                Phase::Local => {
                    if let Some(ratio) = self.tracker.update(dx as f64, dy as f64, pos) {
                        self.begin_handoff(env, ratio);
                    }
                }
                Phase::Pending { .. } => {} // input stays local until the ack
            },
            LocalInput::Button { button, pressed } => {
                if matches!(self.phase, Phase::Active { .. }) {
                    env.send(Message::MouseButton(MouseButtonEvent { button, pressed }));
                }
            }
            LocalInput::Scroll { dx, dy } => {
                if matches!(self.phase, Phase::Active { .. }) {
                    env.send(Message::Scroll(Scroll { dx, dy }));
                }
            }
            LocalInput::Key { hid, pressed } => {
                if pressed {
                    self.held.insert(hid);
                } else {
                    self.held.remove(&hid);
                }
                // Panic hotkey: detected straight from the evdev stream while
                // grabbed, independent of link and peer state.
                if !matches!(self.phase, Phase::Local) && self.panic_triggered() {
                    tracing::warn!("panic hotkey: releasing KVM capture");
                    self.go_local(
                        env,
                        None,
                        "server: local (panic hotkey)",
                        Some("panic hotkey — input returned to this machine".into()),
                    );
                    return;
                }
                if matches!(self.phase, Phase::Active { .. }) {
                    env.send(Message::Key(KeyEvent { hid, pressed }));
                }
            }
        }
    }

    /// A control message received from the client.
    fn on_client_msg(&mut self, env: &dyn ServerEnv, msg: Message) {
        match msg {
            Message::KvmReady(KvmReady { ready }) => {
                self.peer_ready = ready;
                if !ready && !matches!(self.phase, Phase::Local) {
                    self.go_local(
                        env,
                        None,
                        "server: local (Mac not ready)",
                        Some("Mac can no longer inject (permission changed)".into()),
                    );
                }
            }
            Message::KvmAck(KvmAck) => self.on_ack(env),
            Message::KvmHeartbeat(KvmHeartbeat) => {
                if let Phase::Active { last_alive } = &mut self.phase {
                    *last_alive = Instant::now();
                }
            }
            Message::KvmError(KvmError { reason }) => {
                if !matches!(self.phase, Phase::Local) {
                    self.peer_ready = false;
                    self.go_local(
                        env,
                        None,
                        "server: local (Mac injection error)",
                        Some(format!("Mac is not accepting input ({reason})")),
                    );
                }
            }
            Message::KvmReturn(KvmReturn { ratio }) => {
                if matches!(self.phase, Phase::Active { .. }) {
                    self.go_local(env, Some(ratio as f64), "server: local", None);
                }
            }
            _ => {}
        }
    }

    /// The link came up or went down.
    fn on_link(&mut self, env: &dyn ServerEnv, up: bool) {
        // A fresh session must re-advertise readiness before we hand off again.
        self.peer_ready = false;
        if !up {
            // Dead-man's switch: never stay grabbed without a link.
            self.go_local(
                env,
                None,
                "server: local (link lost)",
                matches!(self.phase, Phase::Active { .. })
                    .then(|| "link to Mac dropped while controlling it".into()),
            );
        } else if matches!(self.phase, Phase::Active { .. } | Phase::Pending { .. }) {
            // A replaced session while captured must also release.
            self.go_local(env, None, "server: local (new session)", None);
        }
    }

    /// Daemon is shutting down (SIGTERM/SIGINT): release before exit.
    fn shutdown(&mut self, env: &dyn ServerEnv) {
        self.go_local(env, None, "server: local (shutdown)", None);
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
    let peer_edge: Edge = cfg.peer_edge.into();
    let mut s = Server {
        guard: CaptureGuard::new(capture),
        tracker: EdgeTracker::new(w, h, peer_edge, cfg.edge_threshold),
        ret: None,
        phase: Phase::Local,
        held: HashSet::new(),
        panic: cfg.panic_hotkey.clone(),
        peer_ready: false,
        peer_edge,
        size: (w, h),
    };
    shared.status.feature(
        "kvm",
        format!("server: local (screen {w:.0}x{h:.0}, peer on {peer_edge:?} edge)"),
    );
    let mut link_rx = shared.watch_link();
    let mut shutdown = shared.shutdown_signal();
    let mut ticker = tokio::time::interval(SERVER_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                // Release before the process exits (RAII guard is the backstop).
                s.shutdown(&LiveEnv(&shared));
                return;
            }

            changed = link_rx.changed() => {
                if changed.is_err() {
                    // Shared is going away; release now, the guard ungrabs on drop.
                    s.shutdown(&LiveEnv(&shared));
                    return;
                }
                let up = link_rx.borrow_and_update().is_some();
                s.on_link(&LiveEnv(&shared), up);
            }

            _ = ticker.tick() => s.tick(&LiveEnv(&shared)),

            Some(ev) = input.recv() => s.on_local_input(&LiveEnv(&shared), ev),

            Some(msg) = rx.recv() => s.on_client_msg(&LiveEnv(&shared), msg),
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

struct Client {
    inj: Option<Box<dyn InputInjector>>,
    tracker: EdgeTracker,
    active: bool,
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
    swap: bool,
    ready: bool,
    threshold: f64,
}

fn send(shared: &Shared, msg: Message) {
    if let Some(l) = shared.link() {
        l.send(msg);
    }
}

impl Client {
    fn map_key(&self, hid: u16) -> u16 {
        if self.swap {
            keycode::swap_ctrl_meta(hid)
        } else {
            hid
        }
    }

    fn release_all(&mut self) {
        if let Some(inj) = self.inj.as_mut() {
            for k in std::mem::take(&mut self.keys) {
                let _ = inj.key(k, false);
            }
            for b in std::mem::take(&mut self.buttons) {
                let _ = inj.button(b, false);
            }
        } else {
            self.keys.clear();
            self.buttons.clear();
        }
    }

    /// (Re)create the injector; update readiness + status. Does not send.
    fn acquire(&mut self, shared: &Shared) {
        match shared.backend.input_injector() {
            Ok(i) => {
                let (w, h) = i.screen_size();
                self.tracker = EdgeTracker::new(w, h, self.tracker.edge, self.threshold);
                self.inj = Some(i);
                self.ready = true;
                shared
                    .status
                    .feature("kvm", format!("client: ready (screen {w:.0}x{h:.0})"));
            }
            Err(e) => {
                self.inj = None;
                self.ready = false;
                shared
                    .status
                    .feature("kvm", format!("client: NOT READY — {}", e.0));
            }
        }
    }

    /// Silent readiness refresh (no messages sent). Used on link-up.
    fn probe(&mut self, shared: &Shared) {
        if self.inj.is_some() {
            if let Err(e) = shared.backend.input_injection_ready() {
                self.inj = None;
                self.ready = false;
                shared
                    .status
                    .feature("kvm", format!("client: NOT READY — {}", e.0));
            }
        } else {
            self.acquire(shared);
        }
    }

    /// Periodic runtime re-check. Re-advertises readiness every tick so a lost
    /// or late `KvmReady` self-heals (the server never hands off to a client it
    /// doesn't currently believe is ready).
    fn refresh(&mut self, shared: &Shared) {
        if self.inj.is_some() {
            if let Err(e) = shared.backend.input_injection_ready() {
                self.fail(shared, e.0); // sends KvmError + KvmReady{false}
                return;
            }
        } else {
            self.acquire(shared);
        }
        send(shared, Message::KvmReady(KvmReady { ready: self.ready }));
    }

    /// Injection failed: release everything, drop the injector, tell the
    /// server at once so it releases the grab, and re-probe from scratch.
    fn fail(&mut self, shared: &Shared, reason: String) {
        tracing::warn!("KVM injection failed: {reason}");
        self.release_all();
        self.active = false;
        self.inj = None;
        self.ready = false;
        send(shared, Message::KvmError(KvmError { reason: reason.clone() }));
        send(shared, Message::KvmReady(KvmReady { ready: false }));
        shared
            .status
            .feature("kvm", format!("client: NOT READY — {reason}"));
    }

    /// Warp + modifier resync for a handoff. Errors abort the ack.
    fn try_enter(&mut self, edge: Edge, ratio: f64, mods: u8) -> anyhow::Result<()> {
        {
            let inj = self.inj.as_mut().ok_or_else(|| anyhow::anyhow!("no injector"))?;
            inj.warp_to(edge, ratio)?;
        }
        for (bit, hid) in [
            (modifiers::SHIFT, keycode::HID_LSHIFT),
            (modifiers::CTRL, keycode::HID_LCTRL),
            (modifiers::ALT, keycode::HID_LALT),
            (modifiers::META, keycode::HID_LMETA),
        ] {
            if mods & bit != 0 {
                let hid = self.map_key(hid);
                self.keys.insert(hid);
                self.inj
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no injector"))?
                    .key(hid, true)?;
            }
        }
        Ok(())
    }
}

async fn run_client(shared: Arc<Shared>, mut rx: mpsc::Receiver<Message>) {
    let cfg = shared.cfg.kvm.clone();
    let swap = cfg.swap_ctrl_cmd && cfg!(target_os = "macos");
    let mut c = Client {
        inj: None,
        tracker: EdgeTracker::new(1.0, 1.0, Edge::Left, cfg.edge_threshold),
        active: false,
        keys: HashSet::new(),
        buttons: HashSet::new(),
        swap,
        ready: false,
        threshold: cfg.edge_threshold,
    };
    // Initial probe so `linkerd status` reflects reality immediately.
    c.acquire(&shared);

    let mut link_rx = shared.watch_link();
    let mut reprobe = tokio::time::interval(READY_REPROBE);
    reprobe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heartbeat = tokio::time::interval(CLIENT_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = reprobe.tick() => c.refresh(&shared),

            _ = heartbeat.tick(), if c.active => {
                send(&shared, Message::KvmHeartbeat(KvmHeartbeat));
            }

            changed = link_rx.changed() => {
                if changed.is_err() { return; }
                let up = link_rx.borrow_and_update().is_some();
                if up {
                    // Fresh link: re-probe and advertise current readiness.
                    c.probe(&shared);
                    send(&shared, Message::KvmReady(KvmReady { ready: c.ready }));
                } else if c.active {
                    c.release_all();
                    c.active = false;
                    shared.status.feature("kvm", "client: idle");
                }
            }

            Some(msg) = rx.recv() => match msg {
                Message::KvmEnter(KvmEnter { edge, ratio, modifiers: mods }) => {
                    if c.inj.is_none() {
                        c.acquire(&shared);
                    }
                    // Re-verify we can really inject right now before acking.
                    let can_inject =
                        c.inj.is_some() && shared.backend.input_injection_ready().is_ok();
                    if !can_inject {
                        c.ready = false;
                        send(&shared, Message::KvmReady(KvmReady { ready: false }));
                        shared.status.feature("kvm", "client: refused handoff (cannot inject)");
                        continue;
                    }
                    c.release_all();
                    c.tracker.edge = edge;
                    c.tracker.place(edge, ratio as f64);
                    match c.try_enter(edge, ratio as f64, mods) {
                        Ok(()) => {
                            c.active = true;
                            send(&shared, Message::KvmAck(KvmAck));
                            shared.status.feature("kvm", "client: active (controlled by peer)");
                        }
                        Err(e) => c.fail(&shared, format!("{e:#}")),
                    }
                }
                Message::KvmLeave(_) => {
                    c.release_all();
                    c.active = false;
                    shared.status.feature("kvm", "client: idle");
                }
                Message::MouseMove(MouseMove { dx, dy }) if c.active => {
                    if let Some(inj) = c.inj.as_mut() {
                        if let Err(e) = inj.move_relative(dx, dy) {
                            c.fail(&shared, format!("{e:#}"));
                            continue;
                        }
                    }
                    let pos = c.inj.as_ref().and_then(|i| i.cursor_position());
                    if let Some(ratio) = c.tracker.update(dx as f64, dy as f64, pos) {
                        c.release_all();
                        c.active = false;
                        send(&shared, Message::KvmReturn(KvmReturn { ratio: ratio as f32 }));
                        shared.status.feature("kvm", "client: idle");
                    }
                }
                Message::MouseButton(MouseButtonEvent { button, pressed }) if c.active => {
                    if pressed { c.buttons.insert(button); } else { c.buttons.remove(&button); }
                    if let Some(inj) = c.inj.as_mut() {
                        if let Err(e) = inj.button(button, pressed) {
                            c.fail(&shared, format!("{e:#}"));
                        }
                    }
                }
                Message::Scroll(Scroll { dx, dy }) if c.active => {
                    if let Some(inj) = c.inj.as_mut() {
                        if let Err(e) = inj.scroll(dx, dy) {
                            c.fail(&shared, format!("{e:#}"));
                        }
                    }
                }
                Message::Key(KeyEvent { hid, pressed }) if c.active => {
                    let hid = c.map_key(hid);
                    if pressed { c.keys.insert(hid); } else { c.keys.remove(&hid); }
                    if let Some(inj) = c.inj.as_mut() {
                        if let Err(e) = inj.key(hid, pressed) {
                            c.fail(&shared, format!("{e:#}"));
                        }
                    }
                }
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

    /// Server-side return tracker brings control back without the client:
    /// enter at the Right edge (client's Left), travel into the peer, then
    /// push back out the Left edge.
    #[test]
    fn server_side_return_tracking() {
        // Mirrors `Server::on_ack`: client edge is the opposite of peer_edge.
        let mut ret = EdgeTracker::new(1000.0, 1000.0, Edge::Left, 2.0);
        ret.place(Edge::Left, 0.5);
        // Into the peer (rightward): no return.
        assert_eq!(ret.update(300.0, 0.0, None), None);
        // All the way back out the left edge: control returns.
        assert_eq!(ret.update(-5000.0, 0.0, None), Some(0.5));
    }

    #[test]
    fn panic_matches_either_hand() {
        let (mut s, _g, _env) = server();
        // Right-hand modifiers satisfy a left-configured combo.
        for k in [
            keycode::HID_RCTRL,
            keycode::HID_RALT,
            keycode::HID_RSHIFT,
            keycode::HID_ESCAPE,
        ] {
            s.held.insert(k);
        }
        assert!(s.panic_triggered());
        s.held.remove(&keycode::HID_ESCAPE);
        assert!(!s.panic_triggered());
    }

    // -----------------------------------------------------------------------
    // Server state-machine tests: a fake client + a mock grab interface.
    //
    // Every scenario asserts the end state is *ungrabbed* and that no release
    // leaves the client with stuck keys (the server always sends KvmLeave,
    // which makes the client release everything).
    // -----------------------------------------------------------------------

    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    #[derive(Default)]
    struct GrabState {
        grabbed: bool,
        grabs: u32,
        ungrabs: u32,
        warps: Vec<(Edge, f64)>,
    }

    #[derive(Clone, Default)]
    struct GrabLog(StdArc<StdMutex<GrabState>>);
    impl GrabLog {
        fn grabbed(&self) -> bool {
            self.0.lock().unwrap().grabbed
        }
    }

    struct MockCap(GrabLog);
    impl InputCapture for MockCap {
        fn set_captured(&mut self, c: bool) -> anyhow::Result<()> {
            let mut st = self.0 .0.lock().unwrap();
            st.grabbed = c;
            if c {
                st.grabs += 1;
            } else {
                st.ungrabs += 1;
            }
            Ok(())
        }
        fn screen_size(&self) -> (f64, f64) {
            (1000.0, 1000.0)
        }
        fn warp_to(&mut self, e: Edge, r: f64) -> anyhow::Result<()> {
            self.0 .0.lock().unwrap().warps.push((e, r));
            Ok(())
        }
    }

    #[derive(Default)]
    struct TestState {
        sent: Vec<Message>,
        statuses: Vec<String>,
        notifications: Vec<String>,
        peer_is_client: bool,
    }

    #[derive(Clone, Default)]
    struct TestEnv(StdArc<StdMutex<TestState>>);
    impl TestEnv {
        fn notifications(&self) -> Vec<String> {
            self.0.lock().unwrap().notifications.clone()
        }
        fn sent_count(&self, pred: impl Fn(&Message) -> bool) -> usize {
            self.0.lock().unwrap().sent.iter().filter(|m| pred(m)).count()
        }
    }
    impl ServerEnv for TestEnv {
        fn send(&self, msg: Message) {
            self.0.lock().unwrap().sent.push(msg);
        }
        fn peer_is_client(&self) -> bool {
            self.0.lock().unwrap().peer_is_client
        }
        fn peer_name(&self) -> String {
            "mac".into()
        }
        fn set_status(&self, h: String) {
            self.0.lock().unwrap().statuses.push(h);
        }
        fn notify(&self, body: String) {
            self.0.lock().unwrap().notifications.push(body);
        }
    }

    /// A ready server (peer connected as a client and advertising readiness),
    /// a handle on the grab state, and a fake-client env.
    fn server() -> (Server, GrabLog, TestEnv) {
        let grab = GrabLog::default();
        let env = TestEnv::default();
        env.0.lock().unwrap().peer_is_client = true;
        let s = Server {
            guard: CaptureGuard::new(Box::new(MockCap(grab.clone()))),
            tracker: EdgeTracker::new(1000.0, 1000.0, Edge::Right, 2.0),
            ret: None,
            phase: Phase::Local,
            held: HashSet::new(),
            panic: vec![
                keycode::HID_LCTRL,
                keycode::HID_LALT,
                keycode::HID_LSHIFT,
                keycode::HID_ESCAPE,
            ],
            peer_ready: true,
            peer_edge: Edge::Right,
            size: (1000.0, 1000.0),
        };
        (s, grab, env)
    }

    fn motion(dx: i32) -> LocalInput {
        LocalInput::Motion { dx, dy: 0, pos: None }
    }
    fn is_enter(m: &Message) -> bool {
        matches!(m, Message::KvmEnter(_))
    }
    fn is_leave(m: &Message) -> bool {
        matches!(m, Message::KvmLeave(_))
    }

    /// Drive to the Active (grabbed) state via a proper offer + ack.
    fn activate(s: &mut Server, env: &TestEnv) {
        s.on_local_input(env, motion(5000)); // push past the right edge
        assert!(matches!(s.phase, Phase::Pending { .. }));
        assert_eq!(env.sent_count(is_enter), 1, "exactly one KvmEnter offered");
        s.on_client_msg(env, Message::KvmAck(KvmAck));
        assert!(matches!(s.phase, Phase::Active { .. }));
    }

    #[test]
    fn readiness_gating_blocks_handoff() {
        let (mut s, grab, env) = server();
        s.peer_ready = false; // client has not advertised it can inject
        s.on_local_input(&env, motion(5000));
        assert!(matches!(s.phase, Phase::Local), "no handoff when not ready");
        assert_eq!(env.sent_count(is_enter), 0, "no KvmEnter sent");
        assert!(!grab.grabbed(), "devices never grabbed");
    }

    #[test]
    fn not_a_client_peer_blocks_handoff() {
        let (mut s, grab, env) = server();
        env.0.lock().unwrap().peer_is_client = false;
        s.on_local_input(&env, motion(5000));
        assert!(matches!(s.phase, Phase::Local));
        assert!(!grab.grabbed());
    }

    #[test]
    fn grabs_only_after_ack() {
        let (mut s, grab, env) = server();
        s.on_local_input(&env, motion(5000));
        assert!(matches!(s.phase, Phase::Pending { .. }));
        assert!(!grab.grabbed(), "must NOT grab before the ack");
        s.on_client_msg(&env, Message::KvmAck(KvmAck));
        assert!(grab.grabbed(), "grabs once the client acks");
    }

    #[test]
    fn client_never_acks_aborts_and_stays_local() {
        let (mut s, grab, env) = server();
        s.on_local_input(&env, motion(5000));
        assert!(!grab.grabbed());
        // No ack arrives; force the ack timeout to elapse, then tick.
        s.phase = Phase::Pending {
            since: Instant::now() - ACK_TIMEOUT - Duration::from_millis(50),
            ratio: 0.5,
        };
        s.tick(&env);
        assert!(matches!(s.phase, Phase::Local), "handoff aborted");
        assert!(!grab.grabbed(), "never grabbed → user not locked out");
        assert!(!s.peer_ready, "don't retry into the void");
        assert!(
            env.notifications().iter().any(|n| n.contains("Accessibility")),
            "user is told why: {:?}",
            env.notifications()
        );
    }

    #[test]
    fn client_acks_then_goes_silent_releases() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        assert!(grab.grabbed());
        // Heartbeats stop; force the liveness timeout and tick.
        s.phase = Phase::Active {
            last_alive: Instant::now() - LIVENESS_TIMEOUT - Duration::from_millis(50),
        };
        s.tick(&env);
        assert!(matches!(s.phase, Phase::Local));
        assert!(!grab.grabbed(), "released after heartbeats stopped");
        assert_eq!(env.sent_count(is_leave), 1, "client told to release keys");
        assert!(!env.notifications().is_empty(), "forced release is notified");
    }

    #[test]
    fn heartbeats_keep_capture_alive() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        // A heartbeat refreshes liveness; a tick right after must not release.
        s.on_client_msg(&env, Message::KvmHeartbeat(KvmHeartbeat));
        s.tick(&env);
        assert!(grab.grabbed(), "still controlling while heartbeats arrive");
    }

    #[test]
    fn injection_error_releases_immediately() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        s.on_client_msg(
            &env,
            Message::KvmError(KvmError {
                reason: "Accessibility permission missing".into(),
            }),
        );
        assert!(matches!(s.phase, Phase::Local));
        assert!(!grab.grabbed());
        assert!(!s.peer_ready);
        assert!(env
            .notifications()
            .iter()
            .any(|n| n.contains("Accessibility")));
    }

    #[test]
    fn peer_not_ready_while_captured_releases() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        s.on_client_msg(&env, Message::KvmReady(KvmReady { ready: false }));
        assert!(matches!(s.phase, Phase::Local));
        assert!(!grab.grabbed());
    }

    #[test]
    fn link_drop_while_captured_releases() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        s.on_link(&env, false); // dead-man's switch
        assert!(matches!(s.phase, Phase::Local));
        assert!(!grab.grabbed());
        assert_eq!(env.sent_count(is_leave), 1);
        assert!(!env.notifications().is_empty());
    }

    #[test]
    fn shutdown_while_captured_releases() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        s.shutdown(&env);
        assert!(matches!(s.phase, Phase::Local));
        assert!(!grab.grabbed());
    }

    #[test]
    fn panic_hotkey_with_dead_peer_releases() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        // Peer is "dead": messages go nowhere and it will never respond.
        env.0.lock().unwrap().peer_is_client = false;
        for hid in [
            keycode::HID_LCTRL,
            keycode::HID_LALT,
            keycode::HID_LSHIFT,
            keycode::HID_ESCAPE,
        ] {
            s.on_local_input(&env, LocalInput::Key { hid, pressed: true });
        }
        assert!(matches!(s.phase, Phase::Local), "panic releases locally");
        assert!(!grab.grabbed(), "devices released regardless of the peer");
        assert!(env
            .notifications()
            .iter()
            .any(|n| n.to_lowercase().contains("panic")));
    }

    #[test]
    fn return_with_silent_client_releases() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        // Travel into the peer, then all the way back across the entry edge —
        // WITHOUT the client ever sending KvmReturn.
        s.on_local_input(&env, motion(300));
        s.on_local_input(&env, motion(-5000));
        assert!(matches!(s.phase, Phase::Local), "server tracked the return");
        assert!(!grab.grabbed());
        assert_eq!(env.sent_count(is_leave), 1, "client told to park");
    }

    #[test]
    fn raii_guard_ungrabs_on_drop() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        assert!(grab.grabbed());
        drop(s); // e.g. task aborted or panicked mid-session
        assert!(!grab.grabbed(), "the RAII guard ungrabs on every exit path");
    }

    #[test]
    fn normal_return_does_not_notify() {
        let (mut s, grab, env) = server();
        activate(&mut s, &env);
        s.on_client_msg(&env, Message::KvmReturn(KvmReturn { ratio: 0.5 }));
        assert!(matches!(s.phase, Phase::Local));
        assert!(!grab.grabbed());
        assert!(
            env.notifications().is_empty(),
            "a clean return is silent (no scary banner)"
        );
    }
}
