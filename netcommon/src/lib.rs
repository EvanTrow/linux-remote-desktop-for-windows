//! QUIC setup shared by host and client: self-signed cert generation on the host side,
//! and fingerprint-pinned verification on the client side.
//!
//! Phase 1 MVP trust model: the host generates a self-signed cert on first run and prints
//! its SHA-256 fingerprint; the client is given that fingerprint out-of-band (CLI arg) and
//! pins to it instead of doing full CA-chain validation. Mutual client-cert auth is a Phase 5
//! "hardening" item per PLAN.md, not implemented yet.

use anyhow::{anyhow, Context, Result};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

pub const ALPN: &[u8] = b"rdproto/1";

pub struct HostIdentity {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
}

pub fn fingerprint_hex(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Loads a cert/key pair from `cert_path`/`key_path` if present, otherwise generates a new
/// self-signed cert (valid for the given LAN hostnames/IPs) and persists it there.
pub fn load_or_generate_host_identity(
    cert_path: &Path,
    key_path: &Path,
    subject_alt_names: Vec<String>,
) -> Result<HostIdentity> {
    if cert_path.exists() && key_path.exists() {
        let cert_bytes = std::fs::read(cert_path).context("reading cached host cert")?;
        let key_bytes = std::fs::read(key_path).context("reading cached host key")?;
        return Ok(HostIdentity {
            cert_der: CertificateDer::from(cert_bytes),
            key_der: PrivateKeyDer::try_from(key_bytes)
                .map_err(|e| anyhow!("parsing cached host key: {e}"))?,
        });
    }

    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(subject_alt_names)
            .context("generating self-signed host certificate")?;
    let cert_der = cert.der().clone();
    let key_der = PrivateKeyDer::try_from(key_pair.serialize_der())
        .map_err(|e| anyhow!("serializing generated host key: {e}"))?;

    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(cert_path, &cert_der)?;
    std::fs::write(key_path, key_der.secret_der())?;

    Ok(HostIdentity {
        cert_der,
        key_der,
    })
}

pub fn build_server_endpoint_config(identity: HostIdentity) -> Result<quinn::ServerConfig> {
    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert_der], identity.key_der)?;
    server_crypto.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_uni_streams(0u32.into());
    server_config.transport_config(Arc::new(transport));
    Ok(server_config)
}

/// A rustls server-cert verifier that pins to a single known SHA-256 fingerprint instead of
/// doing CA-chain validation. Fine for a LAN-only, single-host, single-user tool.
#[derive(Debug)]
struct PinnedFingerprintVerifier {
    expected_fingerprint: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedFingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = fingerprint_hex(end_entity);
        if actual.eq_ignore_ascii_case(&self.expected_fingerprint) {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "host certificate fingerprint mismatch: expected {}, got {actual}",
                self.expected_fingerprint
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub fn build_client_endpoint_config(expected_fingerprint: String) -> Result<quinn::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedFingerprintVerifier {
        expected_fingerprint: expected_fingerprint.to_lowercase(),
        provider: provider.clone(),
    });

    let mut client_crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)?;
    Ok(quinn::ClientConfig::new(Arc::new(quic_crypto)))
}
