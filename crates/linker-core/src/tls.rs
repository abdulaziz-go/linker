//! TLS 1.3 / QUIC configuration with mutual authentication against the
//! pinned peer key.
//!
//! Both directions present self-signed certificates. Instead of a CA chain,
//! each side checks the *key fingerprint* of the presented certificate
//! against the trust store (`Trust::accepts`): the paired peer always, an
//! unknown key only while the pairing window is open. The handshake
//! signature is still fully verified, so possession of the private key is
//! proven.

use std::sync::Arc;
use std::time::Duration;

use linker_proto::ALPN;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::identity::{fingerprint_cert, Identity};
use crate::trust::Trust;

/// SNI name used on every connection (ignored by our verifiers).
pub const SERVER_NAME: &str = "linker.local";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct PinnedVerifier {
    trust: Arc<Trust>,
    algs: WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Trust")
    }
}

impl PinnedVerifier {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let fp = fingerprint_cert(end_entity).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        if self.trust.accepts(&fp) {
            Ok(())
        } else {
            tracing::warn!(
                peer = crate::identity::short_id(&fp),
                "rejected unpaired device"
            );
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

impl ClientCertVerifier for PinnedVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// QUIC transport tuning shared by both directions.
fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(
        Duration::from_secs(15)
            .try_into()
            .expect("valid idle timeout"),
    ));
    t.keep_alive_interval(Some(Duration::from_secs(4)));
    t.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    t.datagram_send_buffer_size(4 * 1024 * 1024);
    t.max_concurrent_bidi_streams(64u32.into());
    Arc::new(t)
}

pub fn server_config(
    identity: &Identity,
    trust: Arc<Trust>,
) -> anyhow::Result<quinn::ServerConfig> {
    let p = provider();
    let verifier = Arc::new(PinnedVerifier {
        trust,
        algs: p.signature_verification_algorithms,
    });
    let mut tls = rustls::ServerConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert.clone()], identity.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(transport());
    // Allow the client (dialer) to roam to a new address without reconnecting.
    cfg.migration(true);
    Ok(cfg)
}

pub fn client_config(
    identity: &Identity,
    trust: Arc<Trust>,
) -> anyhow::Result<quinn::ClientConfig> {
    let p = provider();
    let verifier = Arc::new(PinnedVerifier {
        trust,
        algs: p.signature_verification_algorithms,
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert.clone()], identity.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(transport());
    Ok(cfg)
}

/// Fingerprint of the key the peer authenticated with on `conn`.
pub fn peer_fingerprint(conn: &quinn::Connection) -> anyhow::Result<String> {
    let ident = conn
        .peer_identity()
        .ok_or_else(|| anyhow::anyhow!("peer presented no certificate"))?;
    let certs = ident
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow::anyhow!("unexpected peer identity type"))?;
    let first = certs
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty peer certificate chain"))?;
    fingerprint_cert(first)
}
