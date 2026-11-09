//! Peer discovery.
//!
//! * Primary: mDNS / DNS-SD, service type `_linker._udp.local.`, TXT record
//!   with `device_id`, `device_name`, `device_type`, `proto_version`, `port`.
//! * Fallback: a small JSON beacon broadcast on UDP every few seconds, for
//!   networks where multicast is filtered.
//!
//! Discovery is unauthenticated by nature; it only yields *candidate
//! addresses*. Authentication happens in the TLS handshake.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use linker_proto::PROTO_VERSION;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

pub const SERVICE_TYPE: &str = "_linker._udp.local.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    pub device_id: String,
    pub device_name: String,
    pub addrs: Vec<SocketAddr>,
    pub via: &'static str,
}

#[derive(Clone, Debug)]
pub struct SelfInfo {
    pub device_id: String,
    pub device_name: String,
    pub device_type: String,
    pub port: u16,
}

/// Running discovery services. Dropping it unregisters.
pub struct Discovery {
    mdns: Option<ServiceDaemon>,
    fullname: Option<String>,
    tx: mpsc::Sender<Discovered>,
    me: Arc<SelfInfo>,
}

impl Discovery {
    pub async fn start(
        me: SelfInfo,
        use_mdns: bool,
        broadcast_port: Option<u16>,
        tx: mpsc::Sender<Discovered>,
    ) -> Discovery {
        let me = Arc::new(me);
        let mut d = Discovery {
            mdns: None,
            fullname: None,
            tx: tx.clone(),
            me: me.clone(),
        };
        if use_mdns {
            match d.start_mdns() {
                Ok(()) => tracing::info!("mDNS discovery active ({SERVICE_TYPE})"),
                Err(e) => tracing::warn!("mDNS unavailable: {e:#}"),
            }
        }
        if let Some(port) = broadcast_port {
            match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).await {
                Ok(sock) => {
                    if let Err(e) = sock.set_broadcast(true) {
                        tracing::warn!("broadcast discovery disabled: {e}");
                    } else {
                        tracing::info!("UDP broadcast discovery active on port {port}");
                        tokio::spawn(broadcast_loop(Arc::new(sock), port, me, tx));
                    }
                }
                Err(e) => tracing::warn!("broadcast discovery disabled (bind :{port}: {e})"),
            }
        }
        d
    }

    fn start_mdns(&mut self) -> anyhow::Result<()> {
        let daemon = ServiceDaemon::new()?;
        let host = sanitize_host(&gethostname::gethostname().to_string_lossy());
        let instance = format!(
            "linker-{}",
            &self.me.device_id[..16.min(self.me.device_id.len())]
        );
        let props: HashMap<String, String> = [
            ("device_id".to_string(), self.me.device_id.clone()),
            ("device_name".to_string(), self.me.device_name.clone()),
            ("device_type".to_string(), self.me.device_type.clone()),
            ("proto_version".to_string(), PROTO_VERSION.to_string()),
            ("port".to_string(), self.me.port.to_string()),
        ]
        .into_iter()
        .collect();
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &format!("{host}.local."),
            "",
            self.me.port,
            props,
        )?
        .enable_addr_auto();
        self.fullname = Some(info.get_fullname().to_string());
        daemon.register(info)?;
        self.mdns = Some(daemon);
        self.browse()
    }

    fn browse(&self) -> anyhow::Result<()> {
        let Some(daemon) = &self.mdns else {
            return Ok(());
        };
        let rx = daemon.browse(SERVICE_TYPE)?;
        let tx = self.tx.clone();
        let my_id = self.me.device_id.clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv_async().await {
                if let ServiceEvent::ServiceResolved(info) = ev {
                    let Some(id) = info.get_property_val_str("device_id") else {
                        continue;
                    };
                    if id == my_id {
                        continue;
                    }
                    let port = info
                        .get_property_val_str("port")
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(info.get_port());
                    let mut addrs: Vec<SocketAddr> = info
                        .get_addresses()
                        .iter()
                        .filter(|ip| usable(ip))
                        .map(|ip| SocketAddr::new(*ip, port))
                        .collect();
                    // Prefer IPv4 (most home LANs), then link-local v6 last.
                    addrs.sort_by_key(|a| !a.is_ipv4());
                    if addrs.is_empty() {
                        continue;
                    }
                    let d = Discovered {
                        device_id: id.to_string(),
                        device_name: info
                            .get_property_val_str("device_name")
                            .unwrap_or("?")
                            .to_string(),
                        addrs,
                        via: "mdns",
                    };
                    if tx.send(d).await.is_err() {
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    /// Re-query the network (after wake / network change).
    pub fn refresh(&self) {
        if let Some(daemon) = &self.mdns {
            let _ = daemon.stop_browse(SERVICE_TYPE);
            if let Err(e) = self.browse() {
                tracing::debug!("mDNS re-browse failed: {e:#}");
            }
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let (Some(d), Some(name)) = (&self.mdns, &self.fullname) {
            let _ = d.unregister(name);
            let _ = d.shutdown();
        }
    }
}

fn usable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() || cfg!(test),
        // Link-local v6 needs a scope id we don't get from mdns-sd; skip it.
        IpAddr::V6(v6) => !v6.is_loopback() && (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

fn sanitize_host(h: &str) -> String {
    let s: String = h
        .trim_end_matches(".local")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() {
        "linker-host".into()
    } else {
        s
    }
}

#[derive(Serialize, Deserialize)]
struct Beacon {
    linker: u32,
    id: String,
    name: String,
    port: u16,
}

async fn broadcast_loop(
    sock: Arc<UdpSocket>,
    port: u16,
    me: Arc<SelfInfo>,
    tx: mpsc::Sender<Discovered>,
) {
    let beacon = serde_json::to_vec(&Beacon {
        linker: PROTO_VERSION,
        id: me.device_id.clone(),
        name: me.device_name.clone(),
        port: me.port,
    })
    .expect("beacon serializes");
    let sender = {
        let sock = sock.clone();
        tokio::spawn(async move {
            let dst = SocketAddr::from((Ipv4Addr::BROADCAST, port));
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                if let Err(e) = sock.send_to(&beacon, dst).await {
                    tracing::trace!("broadcast send failed: {e}");
                }
            }
        })
    };
    let mut buf = [0u8; 1024];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else {
            break;
        };
        let Ok(b) = serde_json::from_slice::<Beacon>(&buf[..n]) else {
            continue;
        };
        if b.id == me.device_id || b.linker == 0 {
            continue;
        }
        let d = Discovered {
            device_id: b.id,
            device_name: b.name,
            addrs: vec![SocketAddr::new(from.ip(), b.port)],
            via: "broadcast",
        };
        if tx.send(d).await.is_err() {
            break;
        }
    }
    sender.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_sanitized() {
        assert_eq!(sanitize_host("My Mac.local"), "My-Mac");
        assert_eq!(sanitize_host(""), "linker-host");
    }

    #[test]
    fn link_local_v6_skipped() {
        assert!(!usable(&"fe80::1".parse().unwrap()));
        assert!(usable(&"192.168.1.2".parse().unwrap()));
    }
}
