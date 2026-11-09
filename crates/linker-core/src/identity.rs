//! Device identity: a long-lived ECDSA P-256 key pair.
//!
//! `device_id` is the SHA-256 fingerprint (hex) of the public key's
//! SubjectPublicKeyInfo. The X.509 certificate presented on the TLS
//! handshake is self-signed and regenerated from the key on each start; peers
//! pin the *key* fingerprint, so certificate regeneration never breaks pairing.
//!
//! The private key lives in the OS keystore (macOS Keychain, Linux Secret
//! Service) where available, otherwise in a 0600 file in the config dir.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use linker_platform::PlatformBackend;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Paths;

const KEYSTORE_ENTRY: &str = "identity-key";

pub struct Identity {
    pub device_id: String,
    pub device_name: String,
    pub device_type: String,
    pub cert: CertificateDer<'static>,
    key_der: Vec<u8>,
    pub key_location: String,
}

impl Identity {
    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }

    /// Short form for display.
    pub fn short_id(&self) -> &str {
        short_id(&self.device_id)
    }

    /// Build an identity from a PEM key (used by tests and the loader).
    pub fn from_key_pem(
        pem: &str,
        device_name: String,
        key_location: String,
    ) -> anyhow::Result<Identity> {
        let kp = rcgen::KeyPair::from_pem(pem).context("parsing identity key")?;
        let device_id = fingerprint_spki(&kp.public_key_der());
        let mut params = rcgen::CertificateParams::new(vec!["linker.local".to_string()])?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            format!("linker {}", short_id(&device_id)),
        );
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2124, 1, 1);
        let cert = params.self_signed(&kp)?;
        Ok(Identity {
            device_id,
            device_name,
            device_type: device_type().to_string(),
            cert: cert.der().clone(),
            key_der: kp.serialize_der(),
            key_location,
        })
    }

    pub fn generate_pem() -> anyhow::Result<String> {
        Ok(rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?.serialize_pem())
    }
}

pub fn device_type() -> &'static str {
    if cfg!(target_os = "macos") {
        "laptop"
    } else {
        "desktop"
    }
}

pub fn short_id(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// SHA-256 over a DER SubjectPublicKeyInfo, lowercase hex.
pub fn fingerprint_spki(spki_der: &[u8]) -> String {
    hex::encode(Sha256::digest(spki_der))
}

/// Fingerprint of the key inside an X.509 certificate.
pub fn fingerprint_cert(cert_der: &[u8]) -> anyhow::Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| anyhow::anyhow!("bad peer certificate: {e}"))?;
    Ok(fingerprint_spki(cert.public_key().raw))
}

/// Persisted metadata about where the key is.
#[derive(Debug, Serialize, Deserialize)]
struct IdentityMeta {
    device_id: String,
    /// "os" or "file"
    store: String,
}

/// Load the device identity, creating it on first run.
pub async fn load_or_create(
    paths: &Paths,
    keystore_pref: &str,
    device_name: String,
    backend: &Arc<dyn PlatformBackend>,
) -> anyhow::Result<Identity> {
    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("creating {}", paths.config_dir.display()))?;

    if let Ok(s) = std::fs::read_to_string(&paths.identity_file) {
        let meta: IdentityMeta = toml::from_str(&s).context("parsing identity.toml")?;
        let pem = match meta.store.as_str() {
            "os" => load_from_os_with_retry(backend).await?,
            _ => std::fs::read_to_string(&paths.key_file)
                .with_context(|| format!("reading {}", paths.key_file.display()))?,
        };
        let id = Identity::from_key_pem(&pem, device_name, meta.store.clone())?;
        if id.device_id != meta.device_id {
            bail!(
                "identity key does not match identity.toml (expected {}, got {}); \
                 delete {} to start over (you will need to pair again)",
                short_id(&meta.device_id),
                id.short_id(),
                paths.identity_file.display()
            );
        }
        return Ok(id);
    }

    // First run: generate.
    let pem = Identity::generate_pem()?;
    let use_os = match keystore_pref {
        "file" => false,
        "os" => true,
        _ => backend.keystore_available(),
    };
    let mut store = "file";
    if use_os {
        match backend.keystore_set(KEYSTORE_ENTRY, &pem) {
            Ok(()) => store = "os",
            Err(e) if keystore_pref == "os" => return Err(e.context("storing key in OS keystore")),
            Err(e) => tracing::warn!("OS keystore unavailable ({e:#}); using a 0600 key file"),
        }
    }
    if store == "file" {
        write_private_file(&paths.key_file, pem.as_bytes())?;
    }
    let id = Identity::from_key_pem(&pem, device_name, store.to_string())?;
    let meta = IdentityMeta {
        device_id: id.device_id.clone(),
        store: store.into(),
    };
    std::fs::write(&paths.identity_file, toml::to_string(&meta)?)?;
    tracing::info!(
        device_id = id.short_id(),
        store,
        "generated new device identity"
    );
    Ok(id)
}

/// The OS keystore may be locked right after boot (e.g. GNOME keyring before
/// login). Retry for a while rather than generating a new identity, which
/// would silently break pairing.
async fn load_from_os_with_retry(backend: &Arc<dyn PlatformBackend>) -> anyhow::Result<String> {
    let mut delay = Duration::from_secs(2);
    for attempt in 1..=30 {
        match backend.keystore_get(KEYSTORE_ENTRY) {
            Ok(Some(pem)) => return Ok(pem),
            Ok(None) => bail!(
                "identity key missing from the OS keystore; delete identity.toml to create a new \
                 identity (you will need to pair again)"
            ),
            Err(e) => {
                tracing::warn!(attempt, "OS keystore not ready: {e:#}; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
    bail!("OS keystore stayed unavailable")
}

pub fn write_private_file(path: &std::path::Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_fingerprint_matches_key_fingerprint() {
        let pem = Identity::generate_pem().unwrap();
        let id = Identity::from_key_pem(&pem, "x".into(), "file".into()).unwrap();
        assert_eq!(fingerprint_cert(&id.cert).unwrap(), id.device_id);
        assert_eq!(id.device_id.len(), 64);
        // Regenerating the cert from the same key keeps the identity.
        let id2 = Identity::from_key_pem(&pem, "x".into(), "file".into()).unwrap();
        assert_eq!(id.device_id, id2.device_id);
    }

    #[test]
    fn different_keys_different_ids() {
        let a = Identity::from_key_pem(
            &Identity::generate_pem().unwrap(),
            "a".into(),
            "file".into(),
        )
        .unwrap();
        let b = Identity::from_key_pem(
            &Identity::generate_pem().unwrap(),
            "b".into(),
            "file".into(),
        )
        .unwrap();
        assert_ne!(a.device_id, b.device_id);
    }
}
