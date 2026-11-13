//! Pinned-key TLS for QUIC. Each device presents a self-signed certificate made from its
//! ed25519 identity key. Peers are identified by that key alone (the [`DeviceId`]); the
//! certificate chain, names and validity dates carry no meaning.
use anyhow::{Context, Result};
use novalink_proto::DeviceId;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, ring as ring_provider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use x509_parser::oid_registry::OID_SIG_ED25519;

pub const ALPN: &[u8] = b"novalink/1";
/// Idle timeout; our own 100 ms heartbeats keep a live link far below this.
pub const MAX_IDLE: Duration = Duration::from_secs(1);

/// Decides whether a (cryptographically authenticated) peer may connect.
pub trait PeerPolicy: Send + Sync {
    fn allow(&self, peer: &DeviceId) -> bool;
}

/// The [`DeviceId`] of the ed25519 key in a certificate, or `None` for any other key type.
pub fn device_id_of_cert(cert: &CertificateDer<'_>) -> Option<DeviceId> {
    let (_, x509) = x509_parser::parse_x509_certificate(cert.as_ref()).ok()?;
    let spki = &x509.tbs_certificate.subject_pki;
    if spki.algorithm.algorithm != OID_SIG_ED25519 {
        return None;
    }
    let key: [u8; 32] = spki.subject_public_key.data.as_ref().try_into().ok()?;
    Some(DeviceId(key))
}

struct Pinned {
    me: DeviceId,
    policy: Arc<dyn PeerPolicy>,
    algs: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for Pinned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pinned")
            .field("me", &self.me)
            .finish_non_exhaustive()
    }
}

impl Pinned {
    fn check(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), rustls::Error> {
        let deny =
            || rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure);
        if !intermediates.is_empty() {
            return Err(deny());
        }
        let peer = device_id_of_cert(end_entity).ok_or_else(deny)?;
        // Never talk to ourselves (e.g. our own advertisement), whatever the policy says.
        if peer == self.me || !self.policy.allow(&peer) {
            return Err(deny());
        }
        Ok(())
    }

    fn verify_sig(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // Checks the handshake signature against the key inside the pinned certificate.
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

fn no_tls12() -> rustls::Error {
    rustls::Error::General("TLS 1.2 is not supported".into())
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(no_tls12())
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_sig(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn offer_client_auth(&self) -> bool {
        true
    }
    fn client_auth_mandatory(&self) -> bool {
        true
    }
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(no_tls12())
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_sig(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

fn transport() -> Result<Arc<quinn::TransportConfig>> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(MAX_IDLE.try_into().context("idle timeout")?));
    t.keep_alive_interval(None);
    t.datagram_receive_buffer_size(Some(1 << 20));
    Ok(Arc::new(t))
}

/// A QUIC endpoint that both listens and dials, authenticating peers by pinned key.
pub fn endpoint(
    identity: &crate::identity::Identity,
    bind: SocketAddr,
    policy: Arc<dyn PeerPolicy>,
) -> Result<quinn::Endpoint> {
    let me = identity.device_id();
    let key_der = identity.pkcs8_der();
    let key_pair = rcgen::KeyPair::try_from(&PrivatePkcs8KeyDer::from(key_der.clone()))
        .context("load identity key for certificate")?;
    let cert = rcgen::CertificateParams::new(vec!["novalink".to_string()])
        .context("certificate params")?
        .self_signed(&key_pair)
        .context("self-sign certificate")?;
    let chain = vec![cert.der().clone()];
    let key = || PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key_der.clone()));

    let provider = Arc::new(ring_provider::default_provider());
    let verifier = Arc::new(Pinned {
        me,
        policy,
        algs: provider.signature_verification_algorithms,
    });

    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier.clone())
        .with_single_cert(chain.clone(), key())?;
    server.alpn_protocols = vec![ALPN.to_vec()];
    // Resumed handshakes skip certificate verification (and so PeerPolicy); never resume.
    server.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    server.send_tls13_tickets = 0;

    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(chain, key())?;
    client.alpn_protocols = vec![ALPN.to_vec()];
    client.resumption = rustls::client::Resumption::disabled();

    let mut server_cfg =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server)?));
    server_cfg.transport_config(transport()?);
    let mut client_cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client)?));
    client_cfg.transport_config(transport()?);

    let mut ep = quinn::Endpoint::server(server_cfg, bind).context("bind QUIC endpoint")?;
    ep.set_default_client_config(client_cfg);
    Ok(ep)
}
