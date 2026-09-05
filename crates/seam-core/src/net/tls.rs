//! Node identity (self-signed certs) and certificate-fingerprint pinning
//! (Tier 7.6, M8).
//!
//! Both channels are TLS, always — see the crate-level non-negotiable rule.
//! There is no CA here: each node generates its own self-signed certificate
//! on first run (`rcgen`) and identity is established purely by pinning the
//! peer's certificate fingerprint after a human confirms a pairing code
//! (`net::pairing`), the same trust model SSH's `known_hosts` uses. Mutual
//! TLS: both the connecting and the accepting side present a certificate
//! and verify the other's, since this is peer-to-peer with no inherent
//! "server" — see [`client_config`]/[`server_config`].

use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use rcgen::CertifiedKey;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use serde::{Deserialize, Serialize};

/// Errors generating, loading, or applying a node's TLS identity.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// Reading or writing the persisted cert/key files failed.
    #[error("identity I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// `rcgen` failed to generate a fresh self-signed certificate.
    #[error("failed to generate a self-signed certificate: {0}")]
    Generate(String),
    /// Building a `rustls` client/server config from this identity failed.
    #[error("TLS configuration error: {0}")]
    Config(#[from] rustls::Error),
}

/// A 256-bit fingerprint of a certificate's DER encoding. Deliberately
/// BLAKE3 rather than the SHA-256 fingerprint format browsers show —
/// nothing outside this app ever needs to independently compute or verify
/// one, so there's no interop reason to match that convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Hashes `cert`'s DER bytes into a fingerprint.
    fn of_cert(cert: &CertificateDer<'_>) -> Self {
        Self(*blake3::hash(cert.as_ref()).as_bytes())
    }

    /// The raw 32 bytes, for feeding into the pairing-code hash.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase-hex encoding, for display and for `Config`'s TOML storage.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use std::fmt::Write;
        self.0.iter().fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    fn from_hex(s: &str) -> Result<Self, String> {
        if s.len() != 64 {
            return Err(format!(
                "expected a 64-character hex fingerprint, got {} characters",
                s.len()
            ));
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|e| format!("invalid hex fingerprint: {e}"))?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

// Stored as its hex string in TOML — a raw `[u8; 32]` would serialize as an
// unreadable array of 32 integers.
impl Serialize for Fingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// This node's self-signed TLS identity: certificate, private key, and the
/// certificate's own fingerprint (computed once, reused for every
/// connection this process makes).
pub struct NodeIdentity {
    cert_der: CertificateDer<'static>,
    /// PKCS#8 DER-encoded private key. Kept as raw bytes rather than a
    /// `PrivateKeyDer` so a fresh `rustls` config can borrow/clone it
    /// (`PrivateKeyDer` construction from bytes is cheap and side-steps
    /// needing `PrivateKeyDer: Clone`) — see [`Self::private_key`].
    key_pkcs8_der: Vec<u8>,
    /// This identity's own certificate fingerprint — what a peer pinning
    /// against us would store.
    pub fingerprint: Fingerprint,
}

impl NodeIdentity {
    /// Loads a previously-generated identity from `dir`, or generates and
    /// persists a fresh one if none exists yet — the same "generate once on
    /// first run" flow `Config` uses for `node_id` (Tier 7.6).
    ///
    /// # Errors
    /// Returns an error if an existing identity file is corrupt, cert
    /// generation fails, or persisting a fresh identity fails.
    pub fn load_or_create(dir: &Path) -> Result<Self, TlsError> {
        let cert_path = dir.join("identity_cert.der");
        let key_path = dir.join("identity_key.der");

        if let (Ok(cert_bytes), Ok(key_pkcs8_der)) = (fs::read(&cert_path), fs::read(&key_path)) {
            let cert_der = CertificateDer::from(cert_bytes);
            let fingerprint = Fingerprint::of_cert(&cert_der);
            return Ok(Self {
                cert_der,
                key_pkcs8_der,
                fingerprint,
            });
        }

        let identity = Self::generate()?;
        fs::create_dir_all(dir)?;
        fs::write(&cert_path, identity.cert_der.as_ref())?;
        fs::write(&key_path, &identity.key_pkcs8_der)?;
        Ok(identity)
    }

    /// Generates a fresh self-signed identity without touching disk —
    /// mainly for tests, where two in-process nodes each need their own.
    ///
    /// # Errors
    /// Returns an error if `rcgen` fails to generate the certificate.
    pub fn generate() -> Result<Self, TlsError> {
        let CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["seam-node".to_string()])
                .map_err(|e| TlsError::Generate(e.to_string()))?;
        let cert_der = cert.der().clone();
        let key_pkcs8_der = key_pair.serialize_der();
        let fingerprint = Fingerprint::of_cert(&cert_der);
        Ok(Self {
            cert_der,
            key_pkcs8_der,
            fingerprint,
        })
    }

    /// Reconstructs the private key in the shape `rustls` config builders
    /// want. Cheap (a `Vec` clone of the key bytes), called once per config
    /// built — at most a couple of times per process (control + bulk).
    fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.key_pkcs8_der.clone()).into()
    }
}

/// How a `rustls` config decides whether to accept a peer's certificate.
#[derive(Debug, Clone, Copy)]
pub enum Trust {
    /// Accept whatever certificate the peer presents, unconditionally.
    /// Used ONLY for the explicit, human-supervised pairing flow — the
    /// resulting connection is cryptographically authenticated (the peer
    /// really does hold the private key for the cert it presented) but not
    /// yet vetted as "the peer I meant to talk to." That vetting is the
    /// human comparing pairing codes on both screens (`net::pairing`); a
    /// MITM's substituted certificate would produce a different fingerprint
    /// and so a different, visibly-mismatched code.
    OnFirstUse,
    /// Reject any certificate that doesn't match this exact fingerprint —
    /// the normal, everyday connection mode once a peer is paired. A
    /// changed fingerprint (a re-imaged machine, or an actual MITM) hard
    /// fails the handshake rather than silently reconnecting (Tier 7.6).
    Pinned(Fingerprint),
}

/// Builds the `rustls::ClientConfig` for connecting to a peer: presents
/// `identity`'s certificate (mutual TLS — the accepting side verifies us
/// too) and verifies the peer's certificate per `trust`.
///
/// # Errors
/// Returns an error if the underlying `rustls` config builder rejects the
/// certificate/key pair.
pub fn client_config(identity: &NodeIdentity, trust: Trust) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = crypto_provider();
    let verifier = Arc::new(FingerprintVerifier {
        trust,
        provider: provider.clone(),
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert_der.clone()], identity.private_key())?;
    Ok(Arc::new(config))
}

/// Builds the `rustls::ServerConfig` for accepting a connection: presents
/// `identity`'s certificate and REQUIRES the connecting side to present one
/// too (`with_client_cert_verifier`), verified per `trust` — this is what
/// makes it mutual TLS rather than one-directional.
///
/// # Errors
/// Returns an error if the underlying `rustls` config builder rejects the
/// certificate/key pair.
pub fn server_config(identity: &NodeIdentity, trust: Trust) -> Result<Arc<ServerConfig>, TlsError> {
    let provider = crypto_provider();
    let verifier = Arc::new(FingerprintVerifier {
        trust,
        provider: provider.clone(),
    });
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert_der.clone()], identity.private_key())?;
    Ok(Arc::new(config))
}

/// A dummy TLS server-name: this is peer-to-peer with no DNS/CA involved,
/// so SNI is meaningless — [`FingerprintVerifier`] ignores it entirely.
/// Exists only because `rustls`'s client API requires *some* `ServerName`.
///
/// # Panics
/// Never in practice — `"seam-peer"` is a fixed string literal that is
/// statically a valid DNS-style name.
#[must_use]
pub fn dummy_server_name() -> ServerName<'static> {
    ServerName::try_from("seam-peer").expect("\"seam-peer\" is a valid DNS-style name")
}

/// Extracts the peer's certificate fingerprint from an established TLS
/// connection, regardless of which side we were (connecting or accepting).
/// `None` would mean the handshake somehow completed with no peer
/// certificate — unreachable in practice, since both `client_config` and
/// `server_config` make client-cert presentation mandatory, but this stays
/// an `Option` rather than panicking on a `rustls` invariant this module
/// doesn't fully control.
#[must_use]
pub fn peer_fingerprint<T>(stream: &tokio_rustls::TlsStream<T>) -> Option<Fingerprint> {
    let certs = match stream {
        tokio_rustls::TlsStream::Client(s) => s.get_ref().1.peer_certificates(),
        tokio_rustls::TlsStream::Server(s) => s.get_ref().1.peer_certificates(),
    }?;
    let leaf = certs.first()?;
    Some(Fingerprint::of_cert(leaf))
}

/// The process-wide crypto backend, built once and shared by every config
/// this module constructs.
fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    static PROVIDER: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

/// The one certificate verifier this module needs, implementing BOTH
/// `rustls` verifier traits (client-side "is the server who I expect" and
/// server-side "is the connecting client who I expect") since the check
/// itself — does this cert's fingerprint satisfy `trust` — is identical
/// either way (Tier 7.6: fingerprint pinning, not CA chain validation).
#[derive(Debug)]
struct FingerprintVerifier {
    trust: Trust,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl FingerprintVerifier {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        match self.trust {
            Trust::OnFirstUse => Ok(()),
            Trust::Pinned(expected) if Fingerprint::of_cert(end_entity) == expected => Ok(()),
            Trust::Pinned(_) => Err(rustls::Error::General(
                "peer certificate fingerprint does not match the pinned fingerprint".to_string(),
            )),
        }
    }
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ClientCertVerifier for FingerprintVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // No CA, so no hint subjects to advertise — we accept/reject purely
        // on fingerprint after the fact, not on issuer.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Fingerprint, NodeIdentity, Trust, client_config, dummy_server_name, server_config,
    };
    use std::sync::Arc;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    /// Connects `client_identity` to a fresh loopback server presenting
    /// `server_identity`, applying `client_trust`/`server_trust` on the
    /// respective sides. Identities are passed in (rather than generated
    /// inside) so a caller can reuse the SAME server identity across
    /// multiple connection attempts — e.g. to first learn its fingerprint
    /// under `OnFirstUse`, then reconnect `Pinned` to that exact value.
    async fn tls_loopback(
        client_identity: &NodeIdentity,
        client_trust: Trust,
        server_identity: &NodeIdentity,
        server_trust: Trust,
    ) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let server_config = server_config(server_identity, server_trust).expect("server config");
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let acceptor = TlsAcceptor::from(server_config);
            let tls_stream = acceptor.accept(stream).await.expect("server tls accept");
            // Hold the connection open until the client is done with it.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(tls_stream);
        });

        let client_config = client_config(client_identity, client_trust).expect("client config");
        let connector = TlsConnector::from(client_config);
        let stream = TcpStream::connect(addr).await.expect("connect");
        let result = connector.connect(dummy_server_name(), stream).await;

        drop(server_task);
        result
    }

    #[tokio::test]
    async fn on_first_use_connects_to_any_certificate() {
        let client = NodeIdentity::generate().expect("client identity");
        let server = NodeIdentity::generate().expect("server identity");
        let result = tls_loopback(&client, Trust::OnFirstUse, &server, Trust::OnFirstUse).await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn pinned_to_the_correct_fingerprint_connects() {
        let client = NodeIdentity::generate().expect("client identity");
        let server = NodeIdentity::generate().expect("server identity");

        // First contact under OnFirstUse learns the server's real
        // fingerprint (mirrors the pairing flow)...
        let tls_stream = tls_loopback(&client, Trust::OnFirstUse, &server, Trust::OnFirstUse)
            .await
            .expect("first connect");
        let learned_fingerprint =
            super::peer_fingerprint(&tokio_rustls::TlsStream::Client(tls_stream))
                .expect("peer certificate must be present");
        assert_eq!(learned_fingerprint, server.fingerprint);

        // ...then a SEPARATE connection, pinned to exactly that value
        // against the SAME server identity, must succeed.
        let result = tls_loopback(
            &client,
            Trust::Pinned(learned_fingerprint),
            &server,
            Trust::OnFirstUse,
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn pinned_to_the_wrong_fingerprint_hard_fails() {
        let client = NodeIdentity::generate().expect("client identity");
        let server = NodeIdentity::generate().expect("server identity");
        let wrong_fingerprint = NodeIdentity::generate()
            .expect("throwaway identity")
            .fingerprint;

        let result = tls_loopback(
            &client,
            Trust::Pinned(wrong_fingerprint),
            &server,
            Trust::OnFirstUse,
        )
        .await;
        assert!(
            result.is_err(),
            "connecting with a mismatched pinned fingerprint must fail, not silently succeed"
        );
    }

    #[test]
    fn fingerprint_hex_roundtrips() {
        let identity = NodeIdentity::generate().expect("identity");
        let hex = identity.fingerprint.to_hex();
        assert_eq!(hex.len(), 64);
        let parsed = Fingerprint::from_hex(&hex).expect("parse");
        assert_eq!(parsed, identity.fingerprint);
    }

    #[test]
    fn load_or_create_persists_and_reuses_the_same_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = NodeIdentity::load_or_create(dir.path()).expect("create");
        let second = NodeIdentity::load_or_create(dir.path()).expect("load");
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[allow(dead_code)]
    fn assert_send_sync<T: Send + Sync>() {}
    #[allow(dead_code)]
    fn compile_time_checks() {
        assert_send_sync::<Arc<rustls::ClientConfig>>();
    }
}
