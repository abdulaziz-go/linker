//! Configuration (`config.toml`) and on-disk paths.
//!
//! * Linux: `~/.config/linker/config.toml`
//! * macOS: `~/Library/Application Support/linker/config.toml`
//!
//! Every field has a default, so an empty or missing file is valid. The
//! paired peer is stored separately in `paired.toml` (written by the daemon)
//! so the user's hand-edited config is never rewritten.

use std::path::{Path, PathBuf};

use anyhow::Context;
use linker_proto::Edge;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 47800;
pub const DEFAULT_DISCOVERY_PORT: u16 = 47801;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Friendly name shown to the peer. Defaults to the hostname.
    pub device_name: Option<String>,
    /// UDP port for the QUIC transport.
    pub port: u16,
    /// UDP port for the broadcast discovery fallback.
    pub discovery_port: u16,
    /// Enable mDNS / DNS-SD discovery.
    pub mdns: bool,
    /// Enable the UDP broadcast discovery fallback.
    pub broadcast: bool,
    /// Extra peer addresses to try (e.g. `"192.168.1.20:47800"`), for
    /// networks where both discovery mechanisms are blocked.
    pub static_peers: Vec<String>,
    /// Seconds without a link before the one-time "peer not found" notice.
    pub not_found_grace_secs: u64,
    /// Where to keep the private key: "auto", "os" (Keychain / Secret
    /// Service) or "file" (0600 file in the config dir).
    pub keystore: String,
    pub clipboard: ClipboardConfig,
    pub notifications: NotificationsConfig,
    pub dnd: DndConfig,
    pub kvm: KvmConfig,
    pub camera: CameraConfig,
    pub audio: AudioConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            device_name: None,
            port: DEFAULT_PORT,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            mdns: true,
            broadcast: true,
            static_peers: Vec::new(),
            not_found_grace_secs: 60,
            keystore: "auto".into(),
            clipboard: ClipboardConfig::default(),
            notifications: NotificationsConfig::default(),
            dnd: DndConfig::default(),
            kvm: KvmConfig::default(),
            camera: CameraConfig::default(),
            audio: AudioConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClipboardConfig {
    pub enabled: bool,
    /// Payloads up to this many bytes go inline on the control stream;
    /// larger ones are offered and pulled on a separate stream.
    pub inline_threshold: usize,
    /// Never sync payloads larger than this.
    pub max_size: u64,
    /// Skip content marked concealed/transient (password managers).
    pub skip_concealed: bool,
    /// zstd-compress pulled payloads.
    pub compress: bool,
    /// Where received files are written. Default: ~/Downloads/Linker
    pub receive_dir: Option<PathBuf>,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        ClipboardConfig {
            enabled: true,
            inline_threshold: 256 * 1024,
            max_size: 512 * 1024 * 1024,
            skip_concealed: true,
            compress: true,
            receive_dir: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhenDnd {
    /// Write mirrored notifications to the log only.
    Log,
    /// Silently drop them.
    Drop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    pub enabled: bool,
    /// Send this machine's notifications to the peer (where capture exists).
    pub send: bool,
    /// Show notifications received from the peer.
    pub show: bool,
    /// What to do with mirrored banners while DND is on.
    pub when_dnd: WhenDnd,
    /// Apps (bundle ids / names) never mirrored.
    pub ignore_apps: Vec<String>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        NotificationsConfig {
            enabled: true,
            send: true,
            show: true,
            when_dnd: WhenDnd::Log,
            ignore_apps: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DndConfig {
    pub enabled: bool,
    /// Poll interval for platforms without change notifications (ms).
    pub poll_ms: u64,
}

impl Default for DndConfig {
    fn default() -> Self {
        DndConfig {
            enabled: true,
            poll_ms: 1000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvmRole {
    /// Ubuntu = server, macOS = client.
    Auto,
    Server,
    Client,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KvmConfig {
    pub enabled: bool,
    pub role: KvmRole,
    /// (Server) which edge of *this* screen leads to the peer's screen.
    pub peer_edge: EdgeName,
    /// Override the detected screen size (server-side virtual cursor on Wayland).
    pub screen_width: Option<f64>,
    pub screen_height: Option<f64>,
    /// (Client on macOS) map Ctrl ↔ Command so Ctrl+C copies on the Mac.
    pub swap_ctrl_cmd: bool,
    /// Holding all of these keys (HID usage codes) force-releases capture.
    /// Default: Ctrl+Alt+Shift+Escape.
    pub panic_hotkey: Vec<u16>,
    /// Pixels the pointer must push past the edge before switching.
    pub edge_threshold: f64,
}

impl Default for KvmConfig {
    fn default() -> Self {
        KvmConfig {
            enabled: true,
            role: KvmRole::Auto,
            peer_edge: EdgeName::Right,
            screen_width: None,
            screen_height: None,
            swap_ctrl_cmd: true,
            panic_hotkey: vec![0xE0, 0xE2, 0xE1, 0x29],
            edge_threshold: 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeName {
    Left,
    Right,
    Top,
    Bottom,
}

impl From<EdgeName> for Edge {
    fn from(e: EdgeName) -> Edge {
        match e {
            EdgeName::Left => Edge::Left,
            EdgeName::Right => Edge::Right,
            EdgeName::Top => Edge::Top,
            EdgeName::Bottom => Edge::Bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraMode {
    /// Start the Mac camera only while an app on Ubuntu has the device open.
    OnDemand,
    /// Stream whenever the link is up.
    Always,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
    pub enabled: bool,
    /// "auto" (macOS = source, Linux = sink), "source" or "sink".
    pub role: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    pub mode: CameraMode,
    /// (Ubuntu) v4l2loopback device. Default: auto-detect the device whose
    /// card label is "Linker Camera".
    pub video_device: Option<String>,
    /// (Ubuntu) jitter buffer depth in ms.
    pub jitter_ms: u32,
}

impl Default for CameraConfig {
    fn default() -> Self {
        CameraConfig {
            enabled: true,
            role: "auto".into(),
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 4000,
            min_bitrate_kbps: 1000,
            max_bitrate_kbps: 6000,
            mode: CameraMode::OnDemand,
            video_device: None,
            jitter_ms: 20,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioMode {
    /// Run the Mac microphone only while an Ubuntu app records from
    /// "Linker Microphone".
    OnDemand,
    /// Stream the microphone whenever the link is up.
    Always,
}

/// Audio sharing. Each stream is configured on the machine that *plays* it
/// (it requests the stream): the microphone stream's codec/buffer settings
/// come from Ubuntu's file, the speaker stream's from the Mac's file.
/// Device selection is always local.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub enabled: bool,
    /// "auto" (macOS = source: real mic + speakers; Linux = sink: virtual
    /// "Linker Microphone"/"Linker Speaker"), "source" or "sink".
    pub role: String,
    /// Flow A: Mac microphone → Ubuntu "Linker Microphone".
    pub mic: bool,
    /// Flow B: Ubuntu "Linker Speaker" → Mac output.
    pub speaker: bool,
    /// (Ubuntu) when to run the Mac microphone.
    pub mic_mode: AudioMode,
    /// Opus bitrate for streams this machine receives; 0 = automatic
    /// (starts at 128, rises to `max_bitrate_kbps` on a clean network).
    pub bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    /// Opus frame length: 2.5, 5, 10 or 20 ms (smaller = lower latency, more packets).
    pub frame_ms: f64,
    /// Previous frames repeated in each packet (0..=3): this many
    /// consecutive lost packets are recovered without retransmission.
    pub redundancy: u8,
    /// Opus complexity 0..=10.
    pub complexity: u8,
    /// Adaptive jitter buffer bounds (ms) for streams played here.
    pub min_buffer_ms: u32,
    pub max_buffer_ms: u32,
    /// Audio callback period (ms) for devices opened here; 2.67 = 128 frames.
    pub period_ms: f64,
    /// (Mac) microphone to use; default = system default input.
    pub input_device: Option<String>,
    /// (Mac) output to play the speaker stream on; default = system default output.
    pub output_device: Option<String>,
    /// (Ubuntu) names shown to apps.
    pub mic_name: String,
    pub speaker_name: String,
    /// (Ubuntu) PipeWire `node.name`s of the virtual devices (what
    /// `pactl list short sources/sinks` and `--target` use).
    pub mic_node: String,
    pub speaker_node: String,
    /// (Mac) close the output device after this long without speaker audio.
    pub idle_close_secs: u64,
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            enabled: true,
            role: "auto".into(),
            mic: true,
            speaker: true,
            mic_mode: AudioMode::OnDemand,
            bitrate_kbps: 0,
            min_bitrate_kbps: 64,
            max_bitrate_kbps: 256,
            frame_ms: 5.0,
            redundancy: 2,
            complexity: 8,
            min_buffer_ms: 6,
            max_buffer_ms: 200,
            period_ms: 2.67,
            input_device: None,
            output_device: None,
            mic_name: "Linker Microphone".into(),
            speaker_name: "Linker Speaker".into(),
            mic_node: "linker_microphone".into(),
            speaker_node: "linker_speaker".into(),
            idle_close_secs: 10,
        }
    }
}

impl AudioConfig {
    /// Check ranges; returns a human readable problem.
    pub fn validate(&self) -> Result<(), String> {
        if ![2.5, 5.0, 10.0, 20.0].contains(&self.frame_ms) {
            return Err(format!(
                "[audio] frame_ms must be 2.5, 5, 10 or 20 (got {})",
                self.frame_ms
            ));
        }
        if self.redundancy > 3 {
            return Err("[audio] redundancy must be 0..=3".into());
        }
        if self.complexity > 10 {
            return Err("[audio] complexity must be 0..=10".into());
        }
        if self.min_bitrate_kbps < 6 || self.min_bitrate_kbps > self.max_bitrate_kbps {
            return Err("[audio] need 6 <= min_bitrate_kbps <= max_bitrate_kbps".into());
        }
        if self.max_bitrate_kbps > 510 {
            return Err("[audio] max_bitrate_kbps must be <= 510".into());
        }
        if self.bitrate_kbps != 0 && !(6..=510).contains(&self.bitrate_kbps) {
            return Err("[audio] bitrate_kbps must be 0 (auto) or 6..=510".into());
        }
        if self.min_buffer_ms > self.max_buffer_ms || self.max_buffer_ms > 1000 {
            return Err("[audio] need min_buffer_ms <= max_buffer_ms <= 1000".into());
        }
        if !(0.5..=50.0).contains(&self.period_ms) {
            return Err("[audio] period_ms must be 0.5..=50".into());
        }
        if self.idle_close_secs < 2 {
            return Err("[audio] idle_close_secs must be >= 2".into());
        }
        if self.mic_node.is_empty()
            || self.speaker_node.is_empty()
            || self.mic_node == self.speaker_node
        {
            return Err("[audio] mic_node and speaker_node must be non-empty and different".into());
        }
        if !matches!(self.role.as_str(), "auto" | "source" | "sink") {
            return Err("[audio] role must be \"auto\", \"source\" or \"sink\"".into());
        }
        Ok(())
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let c: Config =
                    toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
                c.audio
                    .validate()
                    .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
                Ok(c)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn device_name(&self) -> String {
        self.device_name.clone().unwrap_or_else(|| {
            let h = gethostname::gethostname().to_string_lossy().into_owned();
            h.trim_end_matches(".local").to_string()
        })
    }

    /// The KVM role after resolving `auto`.
    pub fn kvm_role(&self) -> KvmRole {
        match self.kvm.role {
            KvmRole::Auto if cfg!(target_os = "macos") => KvmRole::Client,
            KvmRole::Auto => KvmRole::Server,
            r => r,
        }
    }
}

/// All filesystem locations used by the daemon.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub paired_file: PathBuf,
    pub identity_file: PathBuf,
    pub key_file: PathBuf,
    pub socket: PathBuf,
}

impl Paths {
    pub fn default_paths() -> anyhow::Result<Paths> {
        let base = dirs::config_dir().context("no config directory for this user")?;
        let dir = base.join("linker");
        let socket = if cfg!(target_os = "linux") {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(|d| PathBuf::from(d).join("linker.sock"))
                .unwrap_or_else(|| dir.join("linker.sock"))
        } else {
            dir.join("linker.sock")
        };
        Ok(Paths::in_dir(dir, Some(socket)))
    }

    /// Unix socket paths are limited to ~104 bytes (macOS) / 108 (Linux).
    /// Fall back to a short, per-config-dir path in the temp dir.
    fn short_socket(preferred: PathBuf, dir: &Path) -> PathBuf {
        if preferred.as_os_str().len() < 100 {
            return preferred;
        }
        use sha2::Digest;
        let h = hex::encode(&sha2::Sha256::digest(dir.as_os_str().as_encoded_bytes())[..6]);
        std::env::temp_dir().join(format!("linker-{h}.sock"))
    }

    /// All paths inside `dir` (used by tests and `--config-dir`).
    pub fn in_dir(dir: PathBuf, socket: Option<PathBuf>) -> Paths {
        Paths {
            config_file: dir.join("config.toml"),
            paired_file: dir.join("paired.toml"),
            identity_file: dir.join("identity.toml"),
            key_file: dir.join("identity.key"),
            socket: Self::short_socket(socket.unwrap_or_else(|| dir.join("linker.sock")), &dir),
            config_dir: dir,
        }
    }
}

/// Commented default config written on first run.
pub const DEFAULT_CONFIG_TOML: &str = r#"# Linker configuration. All keys are optional; shown values are defaults.
# Restart the daemon after editing.

# device_name = "my-machine"
# port = 47800              # QUIC (UDP)
# discovery_port = 47801    # UDP broadcast fallback
# mdns = true
# broadcast = true
# static_peers = []         # e.g. ["192.168.1.20:47800"] if discovery is blocked
# not_found_grace_secs = 60
# keystore = "auto"         # "auto" | "os" | "file"

[clipboard]
# enabled = true
# inline_threshold = 262144
# max_size = 536870912
# skip_concealed = true     # don't sync password-manager copies
# compress = true
# receive_dir = "/home/me/Downloads/Linker"

[notifications]
# enabled = true
# send = true               # mirror this machine's notifications (macOS)
# show = true               # show the peer's notifications here
# when_dnd = "log"          # "log" | "drop"
# ignore_apps = []

[dnd]
# enabled = true

[kvm]
# enabled = true
# role = "auto"             # "auto" (Ubuntu=server, Mac=client) | "server" | "client"
# peer_edge = "right"       # server: which edge of THIS screen leads to the peer
# swap_ctrl_cmd = true      # client on macOS: Ctrl acts as Command
# panic_hotkey = [0xE0, 0xE2, 0xE1, 0x29]   # Ctrl+Alt+Shift+Esc
# screen_width = 1920       # override if edge detection is off (Wayland)
# screen_height = 1080

[camera]
# enabled = true
# width = 1280
# height = 720
# fps = 30
# bitrate_kbps = 4000
# mode = "on_demand"        # "on_demand" | "always"
# video_device = "/dev/video10"
# jitter_ms = 20

[audio]
# Flow A: Mac microphone -> Ubuntu "Linker Microphone".
# Flow B: Ubuntu "Linker Speaker" -> Mac speakers/headphones.
# A stream's codec/buffer settings are taken from the machine that PLAYS it.
# enabled = true
# role = "auto"             # "auto" (Mac = source, Ubuntu = sink) | "source" | "sink"
# mic = true
# speaker = true
# mic_mode = "on_demand"    # "on_demand" (Mac mic only while an Ubuntu app records) | "always"
# bitrate_kbps = 0          # 0 = automatic (128, rises to max on a clean network)
# min_bitrate_kbps = 64
# max_bitrate_kbps = 256
# frame_ms = 5              # 2.5 | 5 | 10 | 20
# redundancy = 2            # previous frames repeated per packet (0..3)
# complexity = 8
# min_buffer_ms = 6         # adaptive jitter buffer bounds
# max_buffer_ms = 200
# period_ms = 2.67          # audio callback period (128 frames)
# input_device = "MacBook Pro Microphone"   # Mac; default = system default input
# output_device = "MacBook Pro Speakers"    # Mac; default = system default output
# mic_name = "Linker Microphone"            # Ubuntu: names shown in apps
# speaker_name = "Linker Speaker"
# mic_node = "linker_microphone"            # Ubuntu: PipeWire node names
# speaker_node = "linker_speaker"
# idle_close_secs = 10      # Mac: release the output device when idle (>= 2)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_default() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.port, DEFAULT_PORT);
        assert!(c.clipboard.enabled);
    }

    #[test]
    fn commented_template_parses() {
        let c: Config = toml::from_str(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(c.camera.width, 1280);
        assert!(DEFAULT_CONFIG_TOML.contains("[audio]"));
        assert!(DEFAULT_CONFIG_TOML.contains("# mic_node"));
        c.audio.validate().unwrap();
    }

    #[test]
    fn partial_override() {
        let c: Config =
            toml::from_str("port = 5000\n[kvm]\nrole = \"client\"\npeer_edge = \"left\"\n")
                .unwrap();
        assert_eq!(c.port, 5000);
        assert_eq!(c.kvm.role, KvmRole::Client);
        assert_eq!(Edge::from(c.kvm.peer_edge), Edge::Left);
        assert_eq!(c.kvm_role(), KvmRole::Client);
    }

    #[test]
    fn audio_defaults_and_overrides() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.audio.enabled && c.audio.mic && c.audio.speaker);
        assert_eq!(c.audio.frame_ms, 5.0);
        assert_eq!(c.audio.redundancy, 2);
        assert_eq!(c.audio.mic_mode, AudioMode::OnDemand);
        c.audio.validate().unwrap();
        let c: Config = toml::from_str(
            "[audio]\nframe_ms = 2.5\nbitrate_kbps = 192\nmic_mode = \"always\"\ninput_device = \"USB Mic\"\n",
        )
        .unwrap();
        assert_eq!(c.audio.frame_ms, 2.5);
        assert_eq!(c.audio.bitrate_kbps, 192);
        assert_eq!(c.audio.mic_mode, AudioMode::Always);
        assert_eq!(c.audio.input_device.as_deref(), Some("USB Mic"));
        c.audio.validate().unwrap();
        // integer frame_ms is accepted by TOML as float? (toml requires 5.0 or 5)
        let c: Config = toml::from_str("[audio]\nframe_ms = 10\n").unwrap();
        assert_eq!(c.audio.frame_ms, 10.0);
    }

    #[test]
    fn audio_validation_rejects_bad_values() {
        for bad in [
            "frame_ms = 7",
            "redundancy = 4",
            "complexity = 11",
            "min_buffer_ms = 300\nmax_buffer_ms = 200",
            "bitrate_kbps = 2",
            "role = \"both\"",
            "period_ms = 0",
            "idle_close_secs = 0",
            "mic_node = \"x\"\nspeaker_node = \"x\"",
        ] {
            let c: Config = toml::from_str(&format!("[audio]\n{bad}\n")).unwrap();
            assert!(c.audio.validate().is_err(), "{bad} must be rejected");
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[audio]\nframe_ms = 3\n").unwrap();
        assert!(Config::load(&p).is_err(), "load validates [audio]");
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(toml::from_str::<Config>("prot = 1").is_err());
    }
}
