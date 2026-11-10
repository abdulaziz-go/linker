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

/// Live statistics of one audio stream on this machine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioStats {
    /// "mic" or "speaker".
    pub stream: String,
    /// "send" (this machine captures + encodes) or "recv" (decodes + plays).
    pub direction: String,
    pub state: String,
    /// Local device / virtual node.
    pub device: String,
    /// Native rate of the local device.
    pub sample_rate: u32,
    pub bitrate_kbps: u32,
    pub frame_ms: f64,
    pub redundancy: u8,
    /// Receiver view (for "send" streams: as reported by the peer).
    pub buffer_ms: u32,
    pub target_ms: u32,
    pub peak_gap_ms: u32,
    pub recovered: u64,
    pub plc: u64,
    pub late: u64,
    pub underruns: u64,
    pub trims: u64,
    pub quiet_skips: u64,
    pub speed_pct: f64,
    pub loss_permille: u32,
    /// Capture → playout latency, ms (network part measured with the
    /// heartbeat clock offset + buffer + codec + device period).
    pub latency_ms: f64,
    /// Largest gap between local audio callbacks in the last window, ms.
    pub callback_gap_ms: f64,
    /// Sender view.
    pub packets_per_s: f64,
    pub kbps: f64,
    pub peak_dbfs: String,
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
    /// One entry per audio stream that is set up on this machine.
    #[serde(default)]
    pub audio: Vec<AudioStats>,
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
            audio: Vec::new(),
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

    /// Insert or replace the stats of one audio stream.
    pub fn audio(&self, a: AudioStats) {
        self.update(
            |s| match s.audio.iter_mut().find(|x| x.stream == a.stream) {
                Some(x) => *x = a,
                None => {
                    s.audio.push(a);
                    s.audio.sort_by(|x, y| x.stream.cmp(&y.stream));
                }
            },
        );
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
    for a in &s.audio {
        let _ = writeln!(o, "{}", render_audio(a));
    }
    o
}

/// One audio stream in the style of audiobridge's receiver stats line.
pub fn render_audio(a: &AudioStats) -> String {
    let head = format!("audio {:<8}: {} {}", a.stream, a.direction, a.state);
    let codec = format!(
        "{} kbps {} ms x{}",
        a.bitrate_kbps,
        a.frame_ms,
        a.redundancy as u32 + 1
    );
    if a.direction == "send" {
        format!(
            "{head} | {:.0} pkt/s {:.0} kbps | peak {} | peer buf {}ms (target {}) PLC {} underruns {} | {codec} | latency {:.0} ms | {}",
            a.packets_per_s, a.kbps, a.peak_dbfs, a.buffer_ms, a.target_ms, a.plc, a.underruns, a.latency_ms, a.device
        )
    } else {
        format!(
            "{head} | buf {:3}ms (target {:3}) | gap {:3}ms | recovered {} PLC {} late {} | underruns {} trims {} | speed {:+.1}% | loss {:.1}% | {codec} | latency {:.0} ms | cb {:.1}ms | {}",
            a.buffer_ms,
            a.target_ms,
            a.peak_gap_ms,
            a.recovered,
            a.plc,
            a.late,
            a.underruns,
            a.trims,
            a.speed_pct,
            a.loss_permille as f64 / 10.0,
            a.latency_ms,
            a.callback_gap_ms,
            a.device
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_entries_replace_by_stream() {
        let b = StatusBoard::new("id".into(), "n".into(), "p".into());
        b.audio(AudioStats {
            stream: "speaker".into(),
            ..Default::default()
        });
        b.audio(AudioStats {
            stream: "mic".into(),
            buffer_ms: 1,
            ..Default::default()
        });
        b.audio(AudioStats {
            stream: "mic".into(),
            buffer_ms: 9,
            ..Default::default()
        });
        let s = b.snapshot();
        assert_eq!(s.audio.len(), 2);
        assert_eq!(s.audio[0].stream, "mic");
        assert_eq!(s.audio[0].buffer_ms, 9);
    }

    #[test]
    fn render_lines() {
        let recv = render_audio(&AudioStats {
            stream: "mic".into(),
            direction: "recv".into(),
            state: "playing".into(),
            buffer_ms: 12,
            target_ms: 10,
            speed_pct: 0.25,
            ..Default::default()
        });
        assert!(recv.contains("buf  12ms (target  10)"), "{recv}");
        assert!(
            recv.contains("speed +0.2%") || recv.contains("speed +0.3%"),
            "{recv}"
        );
        let send = render_audio(&AudioStats {
            stream: "speaker".into(),
            direction: "send".into(),
            peak_dbfs: "-inf dBFS".into(),
            ..Default::default()
        });
        assert!(send.contains("peak -inf dBFS"), "{send}");
    }

    #[test]
    fn old_status_json_without_audio_still_parses() {
        let b = StatusBoard::new("id".into(), "n".into(), "p".into());
        let mut v = serde_json::to_value(b.snapshot()).unwrap();
        v.as_object_mut().unwrap().remove("audio");
        let s: Status = serde_json::from_value(v).unwrap();
        assert!(s.audio.is_empty());
    }
}
