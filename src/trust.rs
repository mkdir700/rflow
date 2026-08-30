use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TRUST_STORE_VERSION: u16 = 1;
pub const TRUST_STORE_FILE: &str = "trusted-peers.postcard";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId([u8; 32]);

impl DeviceId {
    pub fn from_certificate(certificate: &[u8]) -> Self {
        Self(Sha256::digest(certificate).into())
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(":")?;
            }
            write!(formatter, "{byte:02X}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub device_id: DeviceId,
    pub display_name: String,
    pub certificate: Vec<u8>,
    pub first_seen_at: u64,
    pub last_seen_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyPeer {
    Unknown(DeviceId),
    Trusted(DeviceId),
    IdentityChanged {
        expected: DeviceId,
        observed: DeviceId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EndpointBinding {
    endpoint: String,
    device_id: DeviceId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustDatabase {
    version: u16,
    peers: Vec<TrustedPeer>,
    endpoints: Vec<EndpointBinding>,
}

impl Default for TrustDatabase {
    fn default() -> Self {
        Self {
            version: TRUST_STORE_VERSION,
            peers: Vec::new(),
            endpoints: Vec::new(),
        }
    }
}

pub struct TrustStore {
    path: PathBuf,
    database: TrustDatabase,
}

impl TrustStore {
    pub fn platform_default() -> Result<Self> {
        Self::load(crate::identity::application_config_directory()?.join(TRUST_STORE_FILE))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let database = match fs::read(&path) {
            Ok(bytes) => postcard::from_bytes::<TrustDatabase>(&bytes)
                .context("decode trusted peer store")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => TrustDatabase::default(),
            Err(error) => return Err(error).context("read trusted peer store"),
        };
        if database.version != TRUST_STORE_VERSION {
            bail!(
                "unsupported trusted peer store version {}",
                database.version
            );
        }
        validate_database(&database)?;
        #[cfg(unix)]
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .context("secure trusted peer store permissions")?;
        }
        Ok(Self { path, database })
    }

    pub fn peers(&self) -> &[TrustedPeer] {
        &self.database.peers
    }

    pub fn remember(
        &mut self,
        display_name: impl Into<String>,
        certificate: &[u8],
        now: u64,
    ) -> Result<DeviceId> {
        let device_id = DeviceId::from_certificate(certificate);
        let mut candidate = self.database.clone();
        if let Some(peer) = candidate
            .peers
            .iter_mut()
            .find(|peer| peer.device_id == device_id)
        {
            if peer.certificate != certificate {
                bail!("trusted peer fingerprint collision");
            }
            peer.display_name = display_name.into();
            peer.last_seen_at = Some(now);
        } else {
            candidate.peers.push(TrustedPeer {
                device_id,
                display_name: display_name.into(),
                certificate: certificate.to_vec(),
                first_seen_at: now,
                last_seen_at: None,
            });
        }
        self.commit(candidate)?;
        Ok(device_id)
    }

    pub fn bind_endpoint(
        &mut self,
        endpoint: impl Into<String>,
        device_id: DeviceId,
    ) -> Result<()> {
        if !self
            .database
            .peers
            .iter()
            .any(|peer| peer.device_id == device_id)
        {
            bail!("cannot bind endpoint to an unknown device");
        }
        let endpoint = endpoint.into();
        let mut candidate = self.database.clone();
        if let Some(binding) = candidate
            .endpoints
            .iter()
            .find(|binding| binding.endpoint == endpoint)
        {
            if binding.device_id != device_id {
                bail!(
                    "endpoint is already bound to {}; forget that device before pairing a replacement",
                    binding.device_id
                );
            }
        } else {
            candidate.endpoints.push(EndpointBinding {
                endpoint,
                device_id,
            });
        }
        self.commit(candidate)
    }

    pub fn verify_endpoint(&self, endpoint: &str, certificate: &[u8]) -> VerifyPeer {
        let observed = DeviceId::from_certificate(certificate);
        match self
            .database
            .endpoints
            .iter()
            .find(|binding| binding.endpoint == endpoint)
        {
            Some(binding) if binding.device_id == observed => VerifyPeer::Trusted(observed),
            Some(binding) => VerifyPeer::IdentityChanged {
                expected: binding.device_id,
                observed,
            },
            None => VerifyPeer::Unknown(observed),
        }
    }

    pub fn verify_certificate(&self, certificate: &[u8]) -> VerifyPeer {
        let observed = DeviceId::from_certificate(certificate);
        if self
            .database
            .peers
            .iter()
            .any(|peer| peer.device_id == observed && peer.certificate == certificate)
        {
            VerifyPeer::Trusted(observed)
        } else {
            VerifyPeer::Unknown(observed)
        }
    }

    pub fn forget(&mut self, device_id: DeviceId) -> Result<bool> {
        let mut candidate = self.database.clone();
        let previous = candidate.peers.len();
        candidate.peers.retain(|peer| peer.device_id != device_id);
        if candidate.peers.len() == previous {
            return Ok(false);
        }
        candidate
            .endpoints
            .retain(|binding| binding.device_id != device_id);
        self.commit(candidate)?;
        Ok(true)
    }

    fn commit(&mut self, candidate: TrustDatabase) -> Result<()> {
        let bytes = postcard::to_allocvec(&candidate).context("encode trusted peer store")?;
        if let Some(directory) = self.path.parent() {
            fs::create_dir_all(directory).context("create trusted peer directory")?;
        }
        let temporary = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove stale temporary trusted peer store"),
        }
        let result: Result<()> = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .apply_private_mode()
                .open(&temporary)
                .context("create temporary trusted peer store")?;
            file.write_all(&bytes)
                .context("write temporary trusted peer store")?;
            file.sync_all().context("sync trusted peer store")?;
            replace_file(&temporary, &self.path)?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result?;
        self.database = candidate;
        Ok(())
    }
}

fn validate_database(database: &TrustDatabase) -> Result<()> {
    for (index, peer) in database.peers.iter().enumerate() {
        if peer.device_id != DeviceId::from_certificate(&peer.certificate) {
            bail!("trusted peer store contains an invalid device fingerprint");
        }
        if database.peers[..index]
            .iter()
            .any(|existing| existing.device_id == peer.device_id)
        {
            bail!("trusted peer store contains duplicate devices");
        }
    }
    for (index, binding) in database.endpoints.iter().enumerate() {
        if !database
            .peers
            .iter()
            .any(|peer| peer.device_id == binding.device_id)
        {
            bail!("trusted peer store endpoint references an unknown device");
        }
        if database.endpoints[..index]
            .iter()
            .any(|existing| existing.endpoint == binding.endpoint)
        {
            bail!("trusted peer store contains duplicate endpoints");
        }
    }
    Ok(())
}

trait PrivateFileMode {
    fn apply_private_mode(&mut self) -> &mut Self;
}

impl PrivateFileMode for fs::OpenOptions {
    fn apply_private_mode(&mut self) -> &mut Self {
        #[cfg(unix)]
        self.mode(0o600);
        self
    }
}

#[cfg(unix)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).context("atomically replace trusted peer store")?;
    if let Some(directory) = destination.parent() {
        fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .context("sync trusted peer directory")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_survives_reload_and_endpoint_identity_changes_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trusted-peers.postcard");
        let mut trust = TrustStore::load(&path).unwrap();
        let server_id = trust
            .remember("linux-desktop", b"server certificate", 1)
            .unwrap();
        trust
            .bind_endpoint("linux-desktop.local:24801", server_id)
            .unwrap();

        let trust = TrustStore::load(&path).unwrap();
        assert_eq!(
            trust.verify_endpoint("linux-desktop.local:24801", b"server certificate"),
            VerifyPeer::Trusted(server_id)
        );
        assert!(matches!(
            trust.verify_endpoint("linux-desktop.local:24801", b"changed certificate"),
            VerifyPeer::IdentityChanged { expected, .. } if expected == server_id
        ));
    }

    #[test]
    fn endpoint_rebinding_requires_forgetting_the_old_device() {
        let directory = tempfile::tempdir().unwrap();
        let mut trust = TrustStore::load(directory.path().join("trust")).unwrap();
        let first = trust.remember("first", b"first", 1).unwrap();
        let second = trust.remember("second", b"second", 1).unwrap();
        trust.bind_endpoint("desktop:24801", first).unwrap();
        let error = trust.bind_endpoint("desktop:24801", second).unwrap_err();
        assert!(error.to_string().contains("forget that device"));
    }
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .context("atomically replace trusted peer store");
    }
    Ok(())
}
