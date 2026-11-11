//! TLS to Manager: certificate pinning by SHA-256 fingerprint.
//!
//! Manager's certificate is self-signed, so there is no CA to check.
//! Instead the Pi pins the SHA-256 of the DER certificate:
//! - Every handshake (pair or key mode) runs with a pin, and any other
//!   certificate fails the TLS handshake, before a proof is sent.
//! - Before the first pairing there is no pin yet. The Pi then only reads
//!   the certificate's fingerprint with `probe_config` (TLS, then close: no
//!   WebSocket, no proof), shows it, and pins it once the owner confirms it
//!   matches the one Sentry Manager shows (contract 2).
//!
//! The handshake signatures are always verified, so the peer really holds
//! the key of the certificate it presented.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::handshake::ct_eq;

pub fn sha256_hex(der: &[u8]) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, der).as_ref())
}

/// Where the verifier leaves the fingerprint it saw, so a mismatch can be
/// logged and reported with it.
pub type SeenSlot = Arc<std::sync::Mutex<Option<String>>>;

#[derive(Debug)]
struct PinVerifier {
    /// `None` only for `probe_config`, which never carries a proof.
    pin: Option<String>,
    provider: Arc<CryptoProvider>,
    seen: SeenSlot,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = sha256_hex(end_entity.as_ref());
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp.clone());
        match &self.pin {
            Some(pin) if ct_eq(&fp, pin) => Ok(ServerCertVerified::assertion()),
            Some(_) => Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)),
            // Fingerprint read only: the connection is closed right after.
            None => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A client config that pins `pin` (lowercase hex): any other certificate
/// fails the TLS handshake. TLS 1.3 and 1.2 only.
pub fn client_config(pin: &str, seen: SeenSlot) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    config(Some(pin.to_string()), seen)
}

/// A client config that accepts any certificate, only to read its
/// fingerprint (`session::read_fingerprint`). Nothing but the TLS handshake
/// ever runs over it.
pub fn probe_config(seen: SeenSlot) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    config(None, seen)
}

fn config(pin: Option<String>, seen: SeenSlot) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinVerifier { pin, provider: provider.clone(), seen });
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}
