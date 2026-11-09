//! Trust store: the pinned fingerprint of the paired peer, plus the
//! temporary "pairing window" during which an unknown key may complete a TLS
//! handshake (and must then pass SAS confirmation before being pinned).

use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedPeer {
    /// Key fingerprint (hex SHA-256 of SPKI). This is what we pin.
    pub device_id: String,
    pub device_name: String,
    pub device_type: String,
    pub paired_at_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct PairedFile {
    peer: Option<PairedPeer>,
}

pub struct Trust {
    path: Option<PathBuf>,
    inner: RwLock<Inner>,
}

struct Inner {
    paired: Option<PairedPeer>,
    pairing_until: Option<Instant>,
}

impl Trust {
    pub fn load(path: PathBuf) -> anyhow::Result<Trust> {
        let paired = match std::fs::read_to_string(&path) {
            Ok(s) => {
                toml::from_str::<PairedFile>(&s)
                    .context("parsing paired.toml")?
                    .peer
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("reading paired.toml"),
        };
        Ok(Trust {
            path: Some(path),
            inner: RwLock::new(Inner {
                paired,
                pairing_until: None,
            }),
        })
    }

    /// In-memory store (tests).
    pub fn ephemeral(paired: Option<PairedPeer>) -> Trust {
        Trust {
            path: None,
            inner: RwLock::new(Inner {
                paired,
                pairing_until: None,
            }),
        }
    }

    pub fn paired(&self) -> Option<PairedPeer> {
        self.inner.read().expect("trust lock").paired.clone()
    }

    pub fn is_pinned(&self, fingerprint: &str) -> bool {
        self.inner
            .read()
            .expect("trust lock")
            .paired
            .as_ref()
            .is_some_and(|p| p.device_id == fingerprint)
    }

    pub fn pairing_open(&self) -> bool {
        self.inner
            .read()
            .expect("trust lock")
            .pairing_until
            .is_some_and(|t| Instant::now() < t)
    }

    /// May a TLS handshake with this key proceed?
    pub fn accepts(&self, fingerprint: &str) -> bool {
        self.is_pinned(fingerprint) || self.pairing_open()
    }

    pub fn open_pairing(&self, dur: Duration) {
        self.inner.write().expect("trust lock").pairing_until = Some(Instant::now() + dur);
    }

    pub fn close_pairing(&self) {
        self.inner.write().expect("trust lock").pairing_until = None;
    }

    pub fn pin(&self, peer: PairedPeer) -> anyhow::Result<()> {
        {
            let mut g = self.inner.write().expect("trust lock");
            g.paired = Some(peer);
            g.pairing_until = None;
        }
        self.save()
    }

    pub fn unpair(&self) -> anyhow::Result<()> {
        self.inner.write().expect("trust lock").paired = None;
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let file = PairedFile {
            peer: self.paired(),
        };
        crate::identity::write_private_file(path, toml::to_string(&file)?.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> PairedPeer {
        PairedPeer {
            device_id: id.into(),
            device_name: "n".into(),
            device_type: "t".into(),
            paired_at_ms: 0,
        }
    }

    #[test]
    fn stranger_rejected_unless_pairing() {
        let t = Trust::ephemeral(Some(peer("aa")));
        assert!(t.accepts("aa"));
        assert!(!t.accepts("bb"));
        t.open_pairing(Duration::from_secs(60));
        assert!(t.accepts("bb"));
        t.close_pairing();
        assert!(!t.accepts("bb"));
    }

    #[test]
    fn persist_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paired.toml");
        let t = Trust::load(path.clone()).unwrap();
        assert!(t.paired().is_none());
        t.pin(peer("cc")).unwrap();
        let t2 = Trust::load(path).unwrap();
        assert_eq!(t2.paired(), Some(peer("cc")));
        t2.unpair().unwrap();
        assert!(t2.paired().is_none());
    }
}
