//! Platform-independent core of Linker.
//!
//! The core never calls OS APIs directly: everything OS-specific goes
//! through the [`PlatformBackend`] object handed to [`Daemon::new`].

pub mod config;
pub mod control;
pub mod discovery;
pub mod features;
pub mod identity;
pub mod link;
pub mod pairing;
pub mod status;
pub mod supervisor;
pub mod tls;
pub mod trust;

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::Context;
use linker_platform::PlatformBackend;
use linker_proto::{Feature, Hello, PROTO_VERSION};
use tokio::sync::watch;

pub use config::{Config, Paths};
use identity::Identity;
use link::Link;
use pairing::{PairEvent, Pairing};
use status::{Status, StatusBoard};
use trust::Trust;

/// State shared by the supervisor, features and control socket.
pub struct Shared {
    pub cfg: Config,
    pub paths: Paths,
    pub identity: Identity,
    pub trust: Arc<Trust>,
    pub backend: Arc<dyn PlatformBackend>,
    pub status: StatusBoard,
    pub pairing: Pairing,
    link_tx: watch::Sender<Option<Link>>,
    shutdown: watch::Sender<bool>,
}

impl Shared {
    /// The live link, if any.
    pub fn link(&self) -> Option<Link> {
        self.link_tx.borrow().clone()
    }

    /// Subscribe to link up/down changes.
    pub fn watch_link(&self) -> watch::Receiver<Option<Link>> {
        self.link_tx.subscribe()
    }

    pub(crate) fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub(crate) fn set_link(&self, link: Option<Link>) {
        self.link_tx.send_replace(link);
    }

    pub fn is_camera_source(&self) -> bool {
        match self.cfg.camera.role.as_str() {
            "source" => true,
            "sink" => false,
            _ => cfg!(target_os = "macos"),
        }
    }

    /// Audio "source" = the machine with the real mic + speakers (Mac);
    /// "sink" = the machine with the virtual devices (Ubuntu).
    pub fn is_audio_source(&self) -> bool {
        match self.cfg.audio.role.as_str() {
            "source" => true,
            "sink" => false,
            _ => cfg!(target_os = "macos"),
        }
    }

    pub fn features(&self) -> Vec<Feature> {
        let c = &self.cfg;
        let mut f = Vec::new();
        if c.clipboard.enabled {
            f.push(Feature::Clipboard);
        }
        if c.notifications.enabled {
            f.push(Feature::Notifications);
        }
        if c.dnd.enabled {
            f.push(Feature::Dnd);
        }
        if c.kvm.enabled {
            f.push(match c.kvm_role() {
                config::KvmRole::Client => Feature::KvmClient,
                _ => Feature::KvmServer,
            });
        }
        if c.camera.enabled {
            f.push(if self.is_camera_source() {
                Feature::CameraSource
            } else {
                Feature::CameraSink
            });
        }
        f
    }

    pub fn hello(&self) -> Hello {
        Hello {
            proto_version: PROTO_VERSION,
            device_id: self.identity.device_id.clone(),
            device_name: self.identity.device_name.clone(),
            device_type: self.identity.device_type.clone(),
            features: self.features(),
        }
    }
}

/// Handle used by the control socket (and tests) to query / drive the daemon.
#[derive(Clone)]
pub struct DaemonHandle(pub Arc<Shared>);

impl DaemonHandle {
    pub fn status(&self) -> Status {
        let s = &self.0;
        let mut st = s.status.snapshot();
        st.paired = s.trust.paired();
        st.pairing_open = s.trust.pairing_open();
        st
    }

    /// Open the pairing window and get a stream of pairing events.
    pub fn pair_begin(&self) -> tokio::sync::mpsc::UnboundedReceiver<PairEvent> {
        let rx = self.0.pairing.begin();
        self.0.trust.open_pairing(pairing::PAIRING_WINDOW);
        rx
    }

    pub fn pair_decide(&self, accept: bool) -> bool {
        self.0.pairing.decide(accept)
    }

    pub fn pair_cancel(&self) {
        self.0.pairing.cancel();
        self.0.trust.close_pairing();
    }

    /// Close the link gracefully (peer is told immediately) and stop the
    /// supervisor; `Daemon::run` returns afterwards.
    pub fn shutdown(&self) {
        self.0.shutdown.send_replace(true);
    }

    pub fn unpair(&self) -> anyhow::Result<()> {
        self.0.trust.unpair()?;
        if let Some(l) = self.0.link() {
            l.conn.close(1u32.into(), b"unpaired");
        }
        Ok(())
    }
}

pub struct Daemon {
    shared: Arc<Shared>,
    control_socket: bool,
}

impl Daemon {
    /// Load identity + trust from `paths` and build the daemon.
    pub async fn new(
        cfg: Config,
        paths: Paths,
        backend: Arc<dyn PlatformBackend>,
    ) -> anyhow::Result<Daemon> {
        let identity =
            identity::load_or_create(&paths, &cfg.keystore, cfg.device_name(), &backend).await?;
        let trust = Arc::new(Trust::load(paths.paired_file.clone())?);
        Ok(Self::with_parts(cfg, paths, backend, identity, trust, true))
    }

    /// Build from explicit parts (tests).
    pub fn with_parts(
        cfg: Config,
        paths: Paths,
        backend: Arc<dyn PlatformBackend>,
        identity: Identity,
        trust: Arc<Trust>,
        control_socket: bool,
    ) -> Daemon {
        let status = StatusBoard::new(
            identity.device_id.clone(),
            identity.device_name.clone(),
            backend.name(),
        );
        let (link_tx, _) = watch::channel(None);
        let (shutdown, _) = watch::channel(false);
        Daemon {
            shared: Arc::new(Shared {
                cfg,
                paths,
                identity,
                trust,
                backend,
                status,
                pairing: Pairing::default(),
                link_tx,
                shutdown,
            }),
            control_socket,
        }
    }

    pub fn handle(&self) -> DaemonHandle {
        DaemonHandle(self.shared.clone())
    }

    /// Run forever (until the task is cancelled).
    pub async fn run(self) -> anyhow::Result<()> {
        let shared = self.shared;
        tracing::info!(
            device = %shared.identity.device_name,
            id = shared.identity.short_id(),
            key = %shared.identity.key_location,
            platform = %shared.backend.name(),
            "linkerd starting"
        );
        match shared.trust.paired() {
            Some(p) => {
                tracing::info!(peer = %p.device_name, id = identity::short_id(&p.device_id), "paired peer")
            }
            None => tracing::warn!("not paired yet — run `linkerd pair` on both machines"),
        }

        let endpoint = make_endpoint(&shared).context("starting QUIC endpoint")?;
        tracing::info!(addr = %endpoint.local_addr()?, "QUIC endpoint listening");

        let (router, router_rx) = link::router();
        features::spawn_all(shared.clone(), router_rx);

        if self.control_socket {
            let handle = DaemonHandle(shared.clone());
            match control::serve(shared.paths.socket.clone(), handle).await {
                Ok(()) => {
                    tracing::info!(socket = %shared.paths.socket.display(), "control socket ready")
                }
                Err(e) => tracing::warn!("control socket unavailable: {e:#}"),
            }
        }

        let result = supervisor::run(shared.clone(), endpoint, router).await;
        if self.control_socket {
            let _ = std::fs::remove_file(&shared.paths.socket);
        }
        result
    }
}

fn make_endpoint(shared: &Shared) -> anyhow::Result<quinn::Endpoint> {
    let server = tls::server_config(&shared.identity, shared.trust.clone())?;
    let client = tls::client_config(&shared.identity, shared.trust.clone())?;
    let port = shared.cfg.port;
    // Dual-stack first, IPv4-only fallback.
    let mut ep = match quinn::Endpoint::server(
        server.clone(),
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
    ) {
        Ok(ep) => ep,
        Err(_) => quinn::Endpoint::server(server, SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
            .with_context(|| format!("binding UDP port {port}"))?,
    };
    ep.set_default_client_config(client);
    Ok(ep)
}
