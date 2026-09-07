//! Development TLS identity, fingerprint pinning, and the per-run join token.
//!
//! `docs/protocol.md`: "For development use server certificate fingerprint
//! pinning and a session join token; do not disable TLS verification globally."
//!
//! * [`DevIdentity`] is a self-signed certificate + key the server presents.
//! * The client is handed only a 32-byte [`Fingerprint`] out of band. Its
//!   custom verifier ([`PinnedServerVerifier`]) accepts exactly the certificate
//!   whose BLAKE3 digest matches, and rejects every other certificate with a
//!   distinct error — no global "accept any cert" mode exists.
//! * [`JoinToken`] is a per-run shared secret checked after the TLS handshake.

use std::sync::Arc;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use serde::{Deserialize, Serialize};

use crate::config::TransportConfig;
use crate::{ALPN, Result, TransportError};

/// BLAKE3 digest of a certificate's DER encoding. The value pinned by the
/// client and compared against the server's presented certificate.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    /// Digest of `der`.
    pub fn of(der: &[u8]) -> Self {
        Self(*blake3::hash(der).as_bytes())
    }

    /// Lowercase hex, for logs and out-of-band transfer.
    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Parses the 64-character lowercase/uppercase hex written by [`Self::to_hex`].
    pub fn from_hex(hex: &str) -> Option<Self> {
        Some(Self(parse_hex32(hex)?))
    }
}

/// Parses exactly 32 bytes of hex (64 chars, whitespace trimmed).
fn parse_hex32(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fingerprint({})", self.to_hex())
    }
}

/// A per-run join secret. Not a durable credential store; regenerated each run.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinToken(pub [u8; 32]);

impl JoinToken {
    /// Draws a fresh token from the operating system CSPRNG.
    pub fn generate() -> Result<Self> {
        let mut out = [0u8; 32];
        SystemRandom::new()
            .fill(&mut out)
            .map_err(|_| TransportError::Tls("operating-system CSPRNG unavailable".into()))?;
        Ok(Self(out))
    }

    /// Lowercase hex of the 32 secret bytes. Only for writing a per-run token
    /// file consumed by a client on the same machine; never log this.
    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Parses a token file written by [`Self::to_hex`].
    pub fn from_hex(hex: &str) -> Option<Self> {
        Some(Self(parse_hex32(hex)?))
    }

    /// Constant-time equality: the comparison time does not depend on where the
    /// first differing byte is.
    pub fn verify(&self, presented: &JoinToken) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(presented.0.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl std::fmt::Debug for JoinToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the token itself.
        write!(f, "JoinToken(redacted)")
    }
}

/// A self-signed development server identity.
#[derive(Clone)]
pub struct DevIdentity {
    cert_der: CertificateDer<'static>,
    key_der: Vec<u8>,
}

impl std::fmt::Debug for DevIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevIdentity")
            .field("fingerprint", &self.fingerprint())
            .finish()
    }
}

impl DevIdentity {
    /// Generates a fresh self-signed certificate for `localhost` / loopback.
    pub fn generate() -> Result<Self> {
        let subject = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        let certified = rcgen::generate_simple_self_signed(subject)
            .map_err(|e| TransportError::Tls(format!("certificate generation: {e}")))?;
        let cert_der = certified.cert.der().clone();
        let key_der = certified.key_pair.serialize_der();
        Ok(Self { cert_der, key_der })
    }

    /// The digest a client pins.
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(self.cert_der.as_ref())
    }

    /// The certificate DER, for tests that need to present a mismatching cert.
    pub fn certificate_der(&self) -> &[u8] {
        self.cert_der.as_ref()
    }

    /// Builds the QUIC server configuration: this identity, ALPN `spall/1`,
    /// TLS 1.3 only, and the transport timers from `cfg`.
    pub fn server_config(&self, cfg: &TransportConfig) -> Result<quinn::ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| TransportError::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(
                vec![self.cert_der.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone())),
            )
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        tls.max_early_data_size = 0;

        let quic =
            QuicServerConfig::try_from(tls).map_err(|e| TransportError::Tls(e.to_string()))?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(quic));
        server.transport_config(Arc::new(transport_timers(cfg)?));
        Ok(server)
    }
}

/// Builds the QUIC client configuration that pins `expected` and rejects every
/// other server certificate.
pub fn client_config(expected: Fingerprint, cfg: &TransportConfig) -> Result<quinn::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedServerVerifier {
        expected,
        provider: provider.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic = QuicClientConfig::try_from(tls).map_err(|e| TransportError::Tls(e.to_string()))?;
    let mut client = quinn::ClientConfig::new(Arc::new(quic));
    client.transport_config(Arc::new(transport_timers(cfg)?));
    Ok(client)
}

fn transport_timers(cfg: &TransportConfig) -> Result<quinn::TransportConfig> {
    let mut tc = quinn::TransportConfig::default();
    let streams = quinn::VarInt::from_u32(cfg.limits.max_bulk_streams + 2);
    tc.max_concurrent_bidi_streams(streams);
    tc.max_concurrent_uni_streams(quinn::VarInt::from_u32(0));
    tc.keep_alive_interval(Some(cfg.keep_alive_interval));
    let idle = quinn::IdleTimeout::try_from(cfg.idle_timeout)
        .map_err(|e| TransportError::Tls(format!("idle timeout out of range: {e}")))?;
    tc.max_idle_timeout(Some(idle));
    Ok(tc)
}

/// A [`ServerCertVerifier`] that accepts exactly one certificate, identified by
/// its BLAKE3 fingerprint. Signatures are still checked with the standard
/// webpki algorithm set.
#[derive(Debug)]
struct PinnedServerVerifier {
    expected: Fingerprint,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if Fingerprint::of(end_entity.as_ref()) == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
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
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_only_the_same_der() {
        let a = DevIdentity::generate().unwrap();
        let b = DevIdentity::generate().unwrap();
        assert_eq!(a.fingerprint(), Fingerprint::of(a.certificate_der()));
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint().to_hex().len(), 64);
    }

    #[test]
    fn join_token_verify_is_value_based() {
        let t = JoinToken::generate().unwrap();
        assert!(t.verify(&t));
        let mut other = t;
        other.0[31] ^= 1;
        assert!(!t.verify(&other));
        // Distinct generations differ.
        assert!(
            !JoinToken::generate()
                .unwrap()
                .verify(&JoinToken::generate().unwrap())
        );
    }

    #[test]
    fn token_debug_is_redacted() {
        let t = JoinToken([7u8; 32]);
        assert_eq!(format!("{t:?}"), "JoinToken(redacted)");
    }

    #[test]
    fn configs_build_with_defaults() {
        let id = DevIdentity::generate().unwrap();
        let cfg = TransportConfig::for_tests();
        id.server_config(&cfg).unwrap();
        client_config(id.fingerprint(), &cfg).unwrap();
    }
}
