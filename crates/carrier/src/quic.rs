//! QUIC configuration of a direct carrier. P is always the client and A always
//! the server. TLS is pinned to A's per-process certificate and hides the
//! `DirectOpen` token, but grants nothing: Noise IK authenticates every
//! session.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use device_proto::candidates::CertHash;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{EndpointConfig, IdleTimeout, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, PeerIncompatible, SignatureScheme};
use zeroize::Zeroizing;

use crate::error::{CarrierError, TlsSide};

pub const DIRECT_QUIC_ALPN: &str = "baybo-direct/1";
/// The name A's certificate is issued for and P's handshake names.
pub const DIRECT_QUIC_SERVER_NAME: &str = "baybo-direct";
/// Under common UDP NAT timeouts.
pub const DIRECT_QUIC_KEEP_ALIVE: Duration = Duration::from_secs(10);
/// Equal to the app pump's inbound liveness timeout.
pub const DIRECT_QUIC_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// Bounds what an admitted, unauthenticated peer can make A buffer per stream.
pub const QUIC_STREAM_RECEIVE_WINDOW: u32 = 256 * 1024;
/// Bounds what an admitted, unauthenticated peer can make A buffer per
/// connection.
pub const QUIC_CONNECTION_RECEIVE_WINDOW: u32 = 1024 * 1024;
/// Concurrent bidirectional streams A grants per connection, above the app's
/// leg fan-out. A's per-connection authentication semaphore is sized by it.
pub const MAX_STREAMS_PER_CONNECTION: u32 = 32;

const _: () = assert!(DIRECT_QUIC_IDLE_TIMEOUT.as_millis() <= u32::MAX as u128);
const IDLE_TIMEOUT_MS: u32 = DIRECT_QUIC_IDLE_TIMEOUT.as_millis() as u32;

/// The one QUIC version either side speaks.
pub(crate) const QUIC_VERSION_1: u32 = 1;
const LONG_HEADER_FORM: u8 = 0x80;
const LONG_PACKET_TYPE_MASK: u8 = 0x30;
const INITIAL_PACKET_TYPE: u8 = 0x00;
/// RFC 9000 §7.2: a client's Initial names a destination connection ID of at
/// least this length, and after a Retry it names A's, which is this long too.
const MIN_INITIAL_DCID_LEN: u8 = 8;

/// A's per-process self-signed certificate for [`DIRECT_QUIC_SERVER_NAME`] and
/// its key. P pins [`ServerIdentity::cert_hash`], which A seals into its
/// answer.
pub struct ServerIdentity {
    certificate: CertificateDer<'static>,
    key: Zeroizing<PrivatePkcs8KeyDer<'static>>,
    cert_hash: CertHash,
}

impl ServerIdentity {
    pub fn generate() -> Result<Self, CarrierError> {
        let certified =
            rcgen::generate_simple_self_signed(vec![DIRECT_QUIC_SERVER_NAME.to_owned()]).map_err(
                |error| CarrierError::Certificate {
                    reason: error.to_string(),
                },
            )?;
        let certificate = certified.cert.der().clone();
        let key = Zeroizing::new(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
        let cert_hash = CertHash::of_certificate(&certificate);
        Ok(Self {
            certificate,
            key,
            cert_hash,
        })
    }

    pub fn cert_hash(&self) -> CertHash {
        self.cert_hash
    }
}

impl fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("cert_hash", &self.cert_hash)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// A's server configuration: TLS 1.3 with [`DIRECT_QUIC_ALPN`], no session
/// tickets, the pinned transport limits (bidirectional streams only, no
/// datagrams, bounded receive windows) and no connection migration, so an
/// admitted connection never moves A's sends to an address outside the
/// allowed set.
pub fn server_config(
    identity: &ServerIdentity,
    provider: Arc<CryptoProvider>,
) -> Result<quinn::ServerConfig, CarrierError> {
    Ok(server_config_with(Arc::new(server_crypto(
        identity, provider,
    )?)))
}

/// P's client configuration for one answer: it accepts only the certificate
/// whose hash is `pinned`, never resumes a session, and accepts no stream or
/// datagram A might open.
pub fn client_config(
    pinned: CertHash,
    provider: Arc<CryptoProvider>,
) -> Result<quinn::ClientConfig, CarrierError> {
    Ok(client_config_with(Arc::new(client_crypto(
        pinned, provider,
    )?)))
}

/// Every carrier endpoint's configuration. With the `grease_quic_bit`
/// transport parameter never advertised, no peer sends this endpoint a packet
/// with the fixed bit clear, so its socket's demux stays unambiguous.
pub(crate) fn endpoint_config() -> EndpointConfig {
    let mut config = EndpointConfig::default();
    config
        .grease_quic_bit(false)
        .supported_versions(vec![QUIC_VERSION_1]);
    config
}

/// Whether a QUIC segment may reach the endpoint. quinn answers two kinds of
/// long-header packet before an `Incoming` exists, where A's admission cannot
/// suppress the answer: one naming another version gets a Version Negotiation
/// packet whatever its size, and an Initial whose destination connection ID
/// is too short gets a CONNECTION_CLOSE. Neither is ever sent between two
/// carrier endpoints, so neither reaches one.
pub(crate) fn reaches_endpoint(segment: &[u8]) -> bool {
    let Some((&first, rest)) = segment.split_first() else {
        return true;
    };
    if first & LONG_HEADER_FORM == 0 {
        return true;
    }
    let Some((version, rest)) = rest.split_first_chunk() else {
        return false;
    };
    if u32::from_be_bytes(*version) != QUIC_VERSION_1 {
        return false;
    }
    first & LONG_PACKET_TYPE_MASK != INITIAL_PACKET_TYPE
        || rest
            .first()
            .is_some_and(|&dcid_len| dcid_len >= MIN_INITIAL_DCID_LEN)
}

fn server_crypto(
    identity: &ServerIdentity,
    provider: Arc<CryptoProvider>,
) -> Result<QuicServerConfig, CarrierError> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| tls_error(TlsSide::Server, error))?
        .with_no_client_auth()
        .with_single_cert(
            vec![identity.certificate.clone()],
            PrivateKeyDer::Pkcs8(identity.key.clone_key()),
        )
        .map_err(|error| tls_error(TlsSide::Server, error))?;
    tls.alpn_protocols = vec![DIRECT_QUIC_ALPN.as_bytes().to_vec()];
    tls.send_tls13_tickets = 0;
    QuicServerConfig::try_from(tls).map_err(|error| tls_error(TlsSide::Server, error))
}

fn client_crypto(
    pinned: CertHash,
    provider: Arc<CryptoProvider>,
) -> Result<QuicClientConfig, CarrierError> {
    let verifier = Arc::new(PinnedServerCert {
        pinned,
        provider: Arc::clone(&provider),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| tls_error(TlsSide::Client, error))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![DIRECT_QUIC_ALPN.as_bytes().to_vec()];
    tls.resumption = rustls::client::Resumption::disabled();
    QuicClientConfig::try_from(tls).map_err(|error| tls_error(TlsSide::Client, error))
}

fn server_config_with(crypto: Arc<dyn quinn::crypto::ServerConfig>) -> quinn::ServerConfig {
    let mut transport = base_transport();
    transport
        .max_concurrent_bidi_streams(VarInt::from_u32(MAX_STREAMS_PER_CONNECTION))
        .stream_receive_window(VarInt::from_u32(QUIC_STREAM_RECEIVE_WINDOW))
        .receive_window(VarInt::from_u32(QUIC_CONNECTION_RECEIVE_WINDOW));
    let mut config = quinn::ServerConfig::with_crypto(crypto);
    config
        .transport_config(Arc::new(transport))
        .migration(false);
    config
}

fn client_config_with(crypto: Arc<dyn quinn::crypto::ClientConfig>) -> quinn::ClientConfig {
    let mut transport = base_transport();
    transport.max_concurrent_bidi_streams(VarInt::from_u32(0));
    let mut config = quinn::ClientConfig::new(crypto);
    config.transport_config(Arc::new(transport));
    config
}

/// What both sides pin: no unidirectional streams and no datagrams (neither
/// side reads them), the keep-alive and the idle timeout.
fn base_transport() -> TransportConfig {
    let mut transport = TransportConfig::default();
    transport
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .datagram_receive_buffer_size(None)
        .keep_alive_interval(Some(DIRECT_QUIC_KEEP_ALIVE))
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(IDLE_TIMEOUT_MS))));
    transport
}

fn tls_error(side: TlsSide, error: impl fmt::Display) -> CarrierError {
    CarrierError::Tls {
        side,
        reason: error.to_string(),
    }
}

/// P's verifier: the end-entity certificate must hash to the pinned value
/// (compared in constant time), no intermediates may be presented, and the
/// TLS 1.3 handshake signature must verify against that certificate.
#[derive(Debug)]
struct PinnedServerCert {
    pinned: CertHash,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if intermediates.is_empty() && CertHash::of_certificate(end_entity) == self.pinned {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            PeerIncompatible::Tls13RequiredForQuic,
        ))
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
mod tests;
