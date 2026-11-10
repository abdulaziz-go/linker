//! Mock platform backend + helpers for in-process two-daemon tests.

#![allow(dead_code)]

pub mod audio_sim;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use linker_core::config::{CameraMode, Config, KvmRole, Paths};
use linker_core::identity::Identity;
use linker_core::trust::{PairedPeer, Trust};
use linker_core::{Daemon, DaemonHandle};
use linker_platform::*;
use tokio::sync::mpsc;

pub use audio_sim::{AudioSim, Kind as AudioKind};

/// Simulated real-time audio devices (see `audio_sim`).
#[derive(Default)]
pub struct MockState {
    pub audio: Arc<AudioSim>,
    pub clip_tx: Option<mpsc::Sender<ClipboardChange>>,
    pub clip_written: Vec<ClipboardContent>,
    pub notif_tx: Option<mpsc::Sender<Notification>>,
    pub shown: Vec<Notification>,
    pub dnd: bool,
    pub dnd_tx: Option<mpsc::Sender<bool>>,
    pub input_tx: Option<mpsc::Sender<LocalInput>>,
    pub captured: bool,
    pub injected: Vec<String>,
    pub cam_tx: Option<mpsc::Sender<CameraEvent>>,
    pub cam_keyframe_requests: u32,
    pub sink_frames: Vec<Vec<u8>>,
    pub sink_consumers: usize,
}

#[derive(Clone, Default)]
pub struct Mock(pub Arc<Mutex<MockState>>);

impl Mock {
    pub fn st(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.0.lock().unwrap()
    }
}

struct MockCapture(Mock);
impl InputCapture for MockCapture {
    fn set_captured(&mut self, c: bool) -> anyhow::Result<()> {
        self.0.st().captured = c;
        Ok(())
    }
    fn screen_size(&self) -> (f64, f64) {
        (1000.0, 1000.0)
    }
    fn warp_to(&mut self, _e: Edge, _r: f64) -> anyhow::Result<()> {
        Ok(())
    }
}

struct Inj(Mock);
impl InputInjector for Inj {
    fn move_relative(&mut self, dx: i32, dy: i32) -> anyhow::Result<()> {
        self.0.st().injected.push(format!("move {dx} {dy}"));
        Ok(())
    }
    fn button(&mut self, b: MouseButton, p: bool) -> anyhow::Result<()> {
        self.0.st().injected.push(format!("button {b:?} {p}"));
        Ok(())
    }
    fn scroll(&mut self, dx: i32, dy: i32) -> anyhow::Result<()> {
        self.0.st().injected.push(format!("scroll {dx} {dy}"));
        Ok(())
    }
    fn key(&mut self, hid: u16, p: bool) -> anyhow::Result<()> {
        self.0.st().injected.push(format!("key {hid:#x} {p}"));
        Ok(())
    }
    fn warp_to(&mut self, e: Edge, r: f64) -> anyhow::Result<()> {
        self.0.st().injected.push(format!("warp {e:?} {r:.2}"));
        Ok(())
    }
    fn cursor_position(&self) -> Option<(f64, f64)> {
        None
    }
    fn screen_size(&self) -> (f64, f64) {
        (800.0, 600.0)
    }
}

struct Src(Mock);
impl CameraSource for Src {
    fn request_keyframe(&self) {
        self.0.st().cam_keyframe_requests += 1;
    }
    fn set_bitrate(&self, _kbps: u32) {}
}
impl Drop for Src {
    fn drop(&mut self) {
        self.0.st().cam_tx = None;
    }
}

struct Sink(Mock);
impl CameraSink for Sink {
    fn write_encoded(&mut self, annexb: &[u8]) -> anyhow::Result<bool> {
        self.0.st().sink_frames.push(annexb.to_vec());
        Ok(true)
    }
    fn write_placeholder(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn consumers(&self) -> usize {
        self.0.st().sink_consumers
    }
    fn reset_decoder(&mut self) {}
    fn device_path(&self) -> String {
        "/dev/null".into()
    }
    fn decoder_name(&self) -> String {
        "mock".into()
    }
}

impl PlatformBackend for Mock {
    fn name(&self) -> String {
        "mock".into()
    }
    fn clipboard_watch(&self, tx: mpsc::Sender<ClipboardChange>) -> Cap<()> {
        self.st().clip_tx = Some(tx);
        Ok(())
    }
    fn clipboard_write(&self, c: ClipboardContent) -> anyhow::Result<()> {
        let tx = {
            let mut s = self.st();
            s.clip_written.push(c.clone());
            s.clip_tx.clone()
        };
        // Like a real OS, our own write shows up as a clipboard change.
        if let Some(tx) = tx {
            let _ = tx.try_send(ClipboardChange {
                content: c,
                concealed: false,
            });
        }
        Ok(())
    }
    fn show_notification(&self, n: &Notification) -> anyhow::Result<()> {
        self.st().shown.push(n.clone());
        Ok(())
    }
    fn watch_notifications(&self, tx: mpsc::Sender<Notification>) -> Cap<()> {
        self.st().notif_tx = Some(tx);
        Ok(())
    }
    fn get_dnd(&self) -> Cap<bool> {
        Ok(self.st().dnd)
    }
    fn set_dnd(&self, on: bool) -> anyhow::Result<()> {
        let tx = {
            let mut s = self.st();
            s.dnd = on;
            s.dnd_tx.clone()
        };
        if let Some(tx) = tx {
            let _ = tx.try_send(on);
        }
        Ok(())
    }
    fn watch_dnd(&self, tx: mpsc::Sender<bool>) -> Cap<()> {
        self.st().dnd_tx = Some(tx);
        Ok(())
    }
    fn input_capture(&self, tx: mpsc::Sender<LocalInput>) -> Cap<Box<dyn InputCapture>> {
        self.st().input_tx = Some(tx);
        Ok(Box::new(MockCapture(self.clone())))
    }
    fn input_injector(&self) -> Cap<Box<dyn InputInjector>> {
        Ok(Box::new(Inj(self.clone())))
    }
    fn camera_source(
        &self,
        _p: CameraParams,
        tx: mpsc::Sender<CameraEvent>,
    ) -> Cap<Box<dyn CameraSource>> {
        self.st().cam_tx = Some(tx);
        Ok(Box::new(Src(self.clone())))
    }
    fn camera_sink(&self, _p: CameraParams) -> Cap<Box<dyn CameraSink>> {
        Ok(Box::new(Sink(self.clone())))
    }
    fn audio_capture(
        &self,
        spec: AudioSpec,
        sink: PcmSink,
        ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        let a = self.st().audio.clone();
        audio_sim::open_producer(a, AudioKind::Capture, spec, sink, ev)
    }
    fn audio_playback(
        &self,
        spec: AudioSpec,
        src: PcmSource,
        ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        let a = self.st().audio.clone();
        audio_sim::open_consumer(a, AudioKind::Playback, spec, src, ev)
    }
    fn virtual_mic(
        &self,
        spec: AudioSpec,
        src: PcmSource,
        ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        let a = self.st().audio.clone();
        audio_sim::open_consumer(a, AudioKind::VirtualMic, spec, src, ev)
    }
    fn virtual_speaker(
        &self,
        spec: AudioSpec,
        sink: PcmSink,
        ev: mpsc::Sender<AudioEvent>,
    ) -> Cap<Box<dyn AudioStream>> {
        let a = self.st().audio.clone();
        audio_sim::open_producer(a, AudioKind::VirtualSpeaker, spec, sink, ev)
    }
}

pub fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub struct Node {
    pub mock: Mock,
    pub handle: DaemonHandle,
    pub task: tokio::task::JoinHandle<()>,
    pub id: String,
    pub port: u16,
    pub _dir: tempfile::TempDir,
}

pub struct NodeSpec {
    pub audio_role: &'static str,
    pub name: &'static str,
    pub key_pem: String,
    pub port: u16,
    pub peer_port: u16,
    pub paired_with: Option<(String, String)>,
    pub kvm_role: KvmRole,
    pub camera_role: &'static str,
}

pub fn identity(pem: &str, name: &str) -> Identity {
    Identity::from_key_pem(pem, name.into(), "file".into()).unwrap()
}

pub fn spawn(spec: NodeSpec) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config {
        port: spec.port,
        mdns: false,
        broadcast: false,
        static_peers: vec![format!("127.0.0.1:{}", spec.peer_port)],
        not_found_grace_secs: 2,
        ..Default::default()
    };
    cfg.kvm.role = spec.kvm_role;
    cfg.camera.role = spec.camera_role.into();
    cfg.camera.mode = CameraMode::OnDemand;
    cfg.audio.role = spec.audio_role.into();
    cfg.clipboard.receive_dir = Some(dir.path().join("recv"));
    let id = identity(&spec.key_pem, spec.name);
    let device_id = id.device_id.clone();
    let trust = Arc::new(Trust::ephemeral(spec.paired_with.map(|(pid, pname)| {
        PairedPeer {
            device_id: pid,
            device_name: pname,
            device_type: "test".into(),
            paired_at_ms: 0,
        }
    })));
    let mock = Mock::default();
    let paths = Paths::in_dir(dir.path().to_path_buf(), None);
    let d = Daemon::with_parts(cfg, paths, Arc::new(mock.clone()), id, trust, false);
    let handle = d.handle();
    let task = tokio::spawn(async move {
        if let Err(e) = d.run().await {
            eprintln!("daemon exited: {e:#}");
        }
    });
    Node {
        mock,
        handle,
        task,
        id: device_id,
        port: spec.port,
        _dir: dir,
    }
}

/// Poll `f` until true or timeout.
pub async fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Two mock backends whose audio simulators share one marker store, so
/// capture → playout latency can be measured across the two nodes.
pub fn mock_pair() -> (Mock, Mock) {
    let a = Mock::default();
    let b = Mock::default();
    let shared = Arc::new(AudioSim::sharing_markers_with(&a.st().audio));
    b.st().audio = shared;
    (a, b)
}

/// Spawn with an explicit mock backend (e.g. to keep audio-device state
/// across a restart of the same node).
pub fn spawn_with(spec: NodeSpec, mock: Mock, tweak: impl FnOnce(&mut Config)) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config {
        port: spec.port,
        mdns: false,
        broadcast: false,
        static_peers: vec![format!("127.0.0.1:{}", spec.peer_port)],
        not_found_grace_secs: 2,
        ..Default::default()
    };
    cfg.kvm.role = spec.kvm_role;
    cfg.camera.role = spec.camera_role.into();
    cfg.camera.mode = CameraMode::OnDemand;
    cfg.audio.role = spec.audio_role.into();
    cfg.clipboard.receive_dir = Some(dir.path().join("recv"));
    tweak(&mut cfg);
    let id = identity(&spec.key_pem, spec.name);
    let device_id = id.device_id.clone();
    let trust = Arc::new(Trust::ephemeral(spec.paired_with.map(|(pid, pname)| {
        PairedPeer {
            device_id: pid,
            device_name: pname,
            device_type: "test".into(),
            paired_at_ms: 0,
        }
    })));
    let paths = Paths::in_dir(dir.path().to_path_buf(), None);
    let d = Daemon::with_parts(cfg, paths, Arc::new(mock.clone()), id, trust, false);
    let handle = d.handle();
    let task = tokio::spawn(async move {
        if let Err(e) = d.run().await {
            eprintln!("daemon exited: {e:#}");
        }
    });
    Node {
        mock,
        handle,
        task,
        id: device_id,
        port: spec.port,
        _dir: dir,
    }
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .with_test_writer()
        .try_init();
}
