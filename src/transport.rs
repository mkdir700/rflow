use std::{fs, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use quinn::{
    ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime,
    TransportConfig, VarInt,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::identity::{IdentityPaths, TLS_SERVER_NAME};

const PAIRING_ALPN: &[u8] = b"rflow-pairing/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TransportMode {
    /// Native QUIC over UDP.
    Quic,
    /// QUIC UDP datagrams encapsulated in ICMP echo messages (Linux, IPv4).
    Icmp,
}

#[derive(Debug)]
struct PairingServerVerifier {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PairingServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        validate_pairing_chain(end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
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

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PairingClientVerifier {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for PairingClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        validate_pairing_chain(end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
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

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn validate_pairing_chain(
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
) -> std::result::Result<(), rustls::Error> {
    if end_entity.is_empty() {
        return Err(rustls::Error::InvalidCertificate(
            CertificateError::BadEncoding,
        ));
    }
    if !intermediates.is_empty() {
        return Err(rustls::Error::InvalidCertificate(
            CertificateError::UnknownIssuer,
        ));
    }
    Ok(())
}

fn pairing_crypto_provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

fn transport_config() -> Result<Arc<TransportConfig>> {
    let mut transport = TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(1)));
    transport.max_idle_timeout(Some(VarInt::from_u32(5_000).into()));
    transport.datagram_receive_buffer_size(Some(256 * 1024));
    transport.datagram_send_buffer_size(256 * 1024);
    Ok(Arc::new(transport))
}

pub fn server_endpoint(bind: SocketAddr, cert_path: &Path, key_path: &Path) -> Result<Endpoint> {
    let cert = CertificateDer::from(fs::read(cert_path).context("read server certificate")?);
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        fs::read(key_path).context("read server private key")?,
    ));
    let mut config =
        ServerConfig::with_single_cert(vec![cert], key).context("configure server certificate")?;
    config.transport_config(transport_config()?);
    Endpoint::server(config, bind).context("bind QUIC server")
}

pub fn client_endpoint(bind: SocketAddr, cert_path: &Path) -> Result<Endpoint> {
    let cert = CertificateDer::from(fs::read(cert_path).context("read trusted certificate")?);
    let mut roots = RootCertStore::empty();
    roots.add(cert).context("trust rflow server certificate")?;
    let mut config =
        ClientConfig::with_root_certificates(Arc::new(roots)).context("configure QUIC client")?;
    config.transport_config(transport_config()?);
    let mut endpoint = Endpoint::client(bind).context("bind QUIC client")?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

pub fn pairing_server_endpoint(bind: SocketAddr, identity: &IdentityPaths) -> Result<Endpoint> {
    pairing_server_endpoint_with_mode(bind, identity, TransportMode::Quic)
}

pub fn pairing_server_endpoint_with_mode(
    bind: SocketAddr,
    identity: &IdentityPaths,
    mode: TransportMode,
) -> Result<Endpoint> {
    let (certificate, private_key) = load_identity(identity)?;
    let provider = pairing_crypto_provider();
    let verifier = Arc::new(PairingClientVerifier {
        provider: provider.clone(),
    });
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![certificate], private_key)
        .context("configure pairing server identity")?;
    tls.alpn_protocols = vec![PAIRING_ALPN.to_vec()];
    let crypto = QuicServerConfig::try_from(tls).context("configure pairing QUIC server")?;
    let mut config = ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport_config()?);
    server_with_mode(config, bind, mode)
}

pub fn pairing_client_endpoint(bind: SocketAddr, identity: &IdentityPaths) -> Result<Endpoint> {
    pairing_client_endpoint_with_mode(bind, identity, TransportMode::Quic)
}

pub fn pairing_client_endpoint_with_mode(
    bind: SocketAddr,
    identity: &IdentityPaths,
    mode: TransportMode,
) -> Result<Endpoint> {
    let (certificate, private_key) = load_identity(identity)?;
    let provider = pairing_crypto_provider();
    let verifier = Arc::new(PairingServerVerifier {
        provider: provider.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![certificate], private_key)
        .context("configure pairing client identity")?;
    tls.alpn_protocols = vec![PAIRING_ALPN.to_vec()];
    let crypto = QuicClientConfig::try_from(tls).context("configure pairing QUIC client")?;
    let mut config = ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport_config()?);
    let mut endpoint = client_with_mode(bind, mode)?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

fn server_with_mode(
    config: ServerConfig,
    bind: SocketAddr,
    mode: TransportMode,
) -> Result<Endpoint> {
    match mode {
        TransportMode::Quic => Endpoint::server(config, bind).context("bind pairing QUIC server"),
        TransportMode::Icmp => Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(config),
            crate::icmp::socket(bind, crate::icmp::Role::Server).context("bind ICMP tunnel")?,
            Arc::new(TokioRuntime),
        )
        .context("create QUIC endpoint over ICMP"),
    }
}

fn client_with_mode(bind: SocketAddr, mode: TransportMode) -> Result<Endpoint> {
    match mode {
        TransportMode::Quic => Endpoint::client(bind).context("bind pairing QUIC client"),
        TransportMode::Icmp => Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            crate::icmp::socket(bind, crate::icmp::Role::Client).context("bind ICMP tunnel")?,
            Arc::new(TokioRuntime),
        )
        .context("create QUIC endpoint over ICMP"),
    }
}

pub fn peer_certificate(connection: &Connection) -> Result<Vec<u8>> {
    let identity = connection
        .peer_identity()
        .context("peer did not present a TLS identity")?;
    let certificates = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow::anyhow!("unsupported TLS peer identity type"))?;
    let certificate = certificates
        .first()
        .context("peer presented an empty certificate chain")?;
    Ok(certificate.as_ref().to_vec())
}

fn load_identity(
    identity: &IdentityPaths,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let certificate = CertificateDer::from(
        fs::read(&identity.certificate).context("read device identity certificate")?,
    );
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        fs::read(&identity.private_key).context("read device identity private key")?,
    ));
    Ok((certificate, private_key))
}

pub async fn accept_one(endpoint: &Endpoint) -> Result<Connection> {
    let incoming = endpoint.accept().await.context("server endpoint closed")?;
    incoming.await.context("accept QUIC connection")
}

pub async fn connect(endpoint: &Endpoint, remote: SocketAddr) -> Result<Connection> {
    endpoint
        .connect(remote, TLS_SERVER_NAME)
        .context("start QUIC connection")?
        .await
        .context("connect to rflow receiver")
}

pub fn generate_identity(cert_path: &Path, key_path: &Path, force: bool) -> Result<()> {
    crate::identity::generate_identity(
        &IdentityPaths {
            certificate: cert_path.to_owned(),
            private_key: key_path.to_owned(),
        },
        force,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn loopback_quic_datagram() {
        let directory = tempfile::tempdir().unwrap();
        let cert = directory.path().join("cert.der");
        let key = directory.path().join("key.der");
        generate_identity(&cert, &key, false).unwrap();

        let server = server_endpoint(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            &cert,
            &key,
        )
        .unwrap();
        let remote = server.local_addr().unwrap();
        let accept = tokio::spawn(async move { accept_one(&server).await.unwrap() });
        let client =
            client_endpoint(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0), &cert).unwrap();
        let sending = connect(&client, remote).await.unwrap();
        let receiving = accept.await.unwrap();
        sending
            .send_datagram(bytes::Bytes::from_static(b"rflow"))
            .unwrap();
        assert_eq!(&receiving.read_datagram().await.unwrap()[..], b"rflow");
    }

    #[tokio::test]
    async fn pairing_transport_proves_both_device_keys_and_exposes_certificates() {
        let directory = tempfile::tempdir().unwrap();
        let server_identity = IdentityPaths::in_directory(directory.path().join("server"));
        let client_identity = IdentityPaths::in_directory(directory.path().join("client"));
        crate::identity::ensure_identity(&server_identity).unwrap();
        crate::identity::ensure_identity(&client_identity).unwrap();
        let server_certificate = fs::read(&server_identity.certificate).unwrap();
        let client_certificate = fs::read(&client_identity.certificate).unwrap();

        let server = pairing_server_endpoint(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            &server_identity,
        )
        .unwrap();
        let remote = server.local_addr().unwrap();
        let accept = tokio::spawn(async move { accept_one(&server).await.unwrap() });
        let client = pairing_client_endpoint(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            &client_identity,
        )
        .unwrap();
        let sending = connect(&client, remote).await.unwrap();
        let receiving = accept.await.unwrap();

        assert_eq!(peer_certificate(&sending).unwrap(), server_certificate);
        assert_eq!(peer_certificate(&receiving).unwrap(), client_certificate);
    }
}
