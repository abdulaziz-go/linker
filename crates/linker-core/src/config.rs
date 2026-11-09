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

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
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
    fn unknown_keys_rejected() {
        assert!(toml::from_str::<Config>("prot = 1").is_err());
    }
}
