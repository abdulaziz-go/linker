//! Observable daemon state, served on the control socket (`linkerd status`).

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::trust::PairedPeer;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LinkState {
    Searching,
    Connecting,
    Connected,
    Pairing,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PeerStatus {
    pub device_id: String,
    pub device_name: String,
    pub device_type: String,
    pub address: String,
    pub rtt_ms: f64,
    pub connected_secs: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CameraStats {
    pub state: String,
    pub fps: f64,
    pub latency_ms: f64,
    pub loss_permille: u32,
    pub bitrate_kbps: u32,
    pub resolution: String,
    pub decoder: String,
    pub device: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub link: LinkState,
    pub peer: Option<PeerStatus>,
    pub paired: Option<PairedPeer>,
    pub pairing_open: bool,
    pub features: BTreeMap<String, String>,
    pub camera: Option<CameraStats>,
}

pub struct StatusBoard(Mutex<Status>);

impl StatusBoard {
    pub fn new(device_id: String, device_name: String, platform: String) -> StatusBoard {
        StatusBoard(Mutex::new(Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            device_id,
            device_name,
            platform,
            link: LinkState::Searching,
            peer: None,
            paired: None,
            pairing_open: false,
            features: BTreeMap::new(),
            camera: None,
        }))
    }

    pub fn update<R>(&self, f: impl FnOnce(&mut Status) -> R) -> R {
        f(&mut self.0.lock().expect("status lock"))
    }

    pub fn snapshot(&self) -> Status {
        self.0.lock().expect("status lock").clone()
    }

    pub fn feature(&self, name: &str, health: impl Into<String>) {
        let health = health.into();
        let changed = self.update(|s| {
            s.features.insert(name.to_string(), health.clone()) != Some(health.clone())
        });
        if changed {
            tracing::info!(feature = name, health = %health, "feature health");
        }
    }

    pub fn link(&self, state: LinkState) {
        self.update(|s| s.link = state);
    }
}

/// Render a status snapshot for humans.
pub fn render(s: &Status) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let _ = writeln!(o, "linkerd {}  ({})", s.version, s.platform);
    let _ = writeln!(
        o,
        "this device : {}  [{}]",
        s.device_name,
        crate::identity::short_id(&s.device_id)
    );
    match &s.paired {
        Some(p) => {
            let _ = writeln!(
                o,
                "paired with : {}  [{}]",
                p.device_name,
                crate::identity::short_id(&p.device_id)
            );
        }
        None => {
            let _ = writeln!(
                o,
                "paired with : (nobody — run `linkerd pair` on both machines)"
            );
        }
    }
    let _ = writeln!(
        o,
        "link        : {:?}{}",
        s.link,
        if s.pairing_open {
            "  (pairing window open)"
        } else {
            ""
        }
    );
    if let Some(p) = &s.peer {
        let _ = writeln!(
            o,
            "peer        : {} at {}  rtt {:.1} ms  up {}s",
            p.device_name, p.address, p.rtt_ms, p.connected_secs
        );
    }
    let _ = writeln!(o, "features:");
    for (k, v) in &s.features {
        let _ = writeln!(o, "  {k:<14} {v}");
    }
    if let Some(c) = &s.camera {
        let _ = writeln!(
            o,
            "camera      : {}  {}  {:.1} fps  latency {:.0} ms  loss {:.1}%  {} kbps  decoder {}  {}",
            c.state,
            c.resolution,
            c.fps,
            c.latency_ms,
            c.loss_permille as f64 / 10.0,
            c.bitrate_kbps,
            c.decoder,
            c.device
        );
    }
    o
}
