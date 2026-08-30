use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result, bail};
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig, TransportConfig, VarInt};
use rustls::RootCertStore;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub const SERVER_NAME: &str = "rflow.local";

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

pub async fn accept_one(endpoint: &Endpoint) -> Result<Connection> {
    let incoming = endpoint.accept().await.context("server endpoint closed")?;
    incoming.await.context("accept QUIC connection")
}

pub async fn connect(endpoint: &Endpoint, remote: SocketAddr) -> Result<Connection> {
    endpoint
        .connect(remote, SERVER_NAME)
        .context("start QUIC connection")?
        .await
        .context("connect to rflow receiver")
}

pub fn generate_identity(cert_path: &Path, key_path: &Path, force: bool) -> Result<()> {
    if !force && (cert_path.exists() || key_path.exists()) {
        bail!("certificate or key already exists; pass --force to overwrite");
    }
    let identity = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()])?;
    fs::write(cert_path, identity.cert.der()).context("write certificate")?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut key_file = options.open(key_path).context("create private key")?;
    key_file
        .write_all(&identity.signing_key.serialize_der())
        .context("write private key")?;
    #[cfg(unix)]
    key_file
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .context("secure private key permissions")?;
    Ok(())
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
}
