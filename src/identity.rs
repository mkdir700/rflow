use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result, bail};

pub const CERTIFICATE_FILE: &str = "identity-cert.der";
pub const PRIVATE_KEY_FILE: &str = "identity-key.der";
pub const TLS_SERVER_NAME: &str = "rflow.local";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPaths {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}

impl IdentityPaths {
    pub fn in_directory(directory: impl AsRef<Path>) -> Self {
        Self {
            certificate: directory.as_ref().join(CERTIFICATE_FILE),
            private_key: directory.as_ref().join(PRIVATE_KEY_FILE),
        }
    }

    pub fn platform_default() -> Result<Self> {
        let directory = application_config_directory()?;
        fs::create_dir_all(&directory).context("create identity directory")?;
        secure_directory(&directory)?;
        Ok(Self::in_directory(directory))
    }
}

pub fn resolve_identity_paths(
    certificate: Option<PathBuf>,
    private_key: Option<PathBuf>,
) -> Result<IdentityPaths> {
    match (certificate, private_key) {
        (Some(certificate), Some(private_key)) => Ok(IdentityPaths {
            certificate,
            private_key,
        }),
        (None, None) => IdentityPaths::platform_default(),
        _ => bail!("--identity-cert and --identity-key must be provided together"),
    }
}

pub fn ensure_identity(paths: &IdentityPaths) -> Result<()> {
    match (paths.certificate.exists(), paths.private_key.exists()) {
        (true, true) => {
            secure_private_key(&paths.private_key)?;
            return Ok(());
        }
        (true, false) | (false, true) => {
            bail!("device identity is incomplete; certificate and private key must both exist")
        }
        (false, false) => {}
    }

    for directory in [paths.certificate.parent(), paths.private_key.parent()]
        .into_iter()
        .flatten()
    {
        fs::create_dir_all(directory).context("create identity directory")?;
    }

    let identity = rcgen::generate_simple_self_signed(vec![TLS_SERVER_NAME.into()])?;
    write_identity_pair(
        paths,
        identity.cert.der(),
        &identity.signing_key.serialize_der(),
    )
}

pub fn generate_identity(paths: &IdentityPaths, force: bool) -> Result<()> {
    if !force && (paths.certificate.exists() || paths.private_key.exists()) {
        bail!("certificate or key already exists; pass --force to overwrite");
    }
    if force {
        remove_if_present(&paths.certificate)?;
        remove_if_present(&paths.private_key)?;
    }
    ensure_identity(paths)
}

fn write_identity_pair(
    paths: &IdentityPaths,
    certificate: &[u8],
    private_key: &[u8],
) -> Result<()> {
    let suffix = format!("tmp-{}", std::process::id());
    let certificate_temp = paths.certificate.with_extension(&suffix);
    let private_key_temp = paths.private_key.with_extension(&suffix);
    let result = (|| {
        write_new_file(&certificate_temp, certificate, false)?;
        write_new_file(&private_key_temp, private_key, true)?;
        fs::rename(&private_key_temp, &paths.private_key).context("install private key")?;
        if let Err(error) = fs::rename(&certificate_temp, &paths.certificate) {
            let _ = fs::remove_file(&paths.private_key);
            return Err(error).context("install certificate");
        }
        Ok(())
    })();
    let _ = fs::remove_file(&certificate_temp);
    let _ = fs::remove_file(&private_key_temp);
    result
}

fn write_new_file(path: &Path, contents: &[u8], private: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .context("create temporary identity file")?;
    file.write_all(contents).context("write identity file")?;
    file.sync_all().context("sync identity file")?;
    #[cfg(unix)]
    if private {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("secure private key permissions")?;
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(unix)]
fn secure_private_key(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .context("secure private key permissions")
}

#[cfg(not(unix))]
fn secure_private_key(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .context("secure identity directory")
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn application_config_directory() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(directory).join("rflow"));
    }
    let home = env::var_os("HOME").context("HOME is not set; cannot locate rflow configuration")?;
    Ok(PathBuf::from(home).join(".config/rflow"))
}

#[cfg(target_os = "macos")]
pub fn application_config_directory() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set; cannot locate rflow configuration")?;
    Ok(PathBuf::from(home).join("Library/Application Support/rflow"))
}

#[cfg(target_os = "windows")]
pub fn application_config_directory() -> Result<PathBuf> {
    let app_data =
        env::var_os("APPDATA").context("APPDATA is not set; cannot locate rflow configuration")?;
    Ok(PathBuf::from(app_data).join("rflow"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn application_config_directory() -> Result<PathBuf> {
    bail!("automatic identity storage is unsupported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_reuses_a_complete_identity() {
        let directory = tempfile::tempdir().unwrap();
        let paths = IdentityPaths::in_directory(directory.path());
        ensure_identity(&paths).unwrap();
        let certificate = fs::read(&paths.certificate).unwrap();
        let private_key = fs::read(&paths.private_key).unwrap();
        ensure_identity(&paths).unwrap();
        assert_eq!(fs::read(&paths.certificate).unwrap(), certificate);
        assert_eq!(fs::read(&paths.private_key).unwrap(), private_key);
    }

    #[test]
    fn refuses_an_incomplete_identity() {
        let directory = tempfile::tempdir().unwrap();
        let paths = IdentityPaths::in_directory(directory.path());
        fs::write(&paths.certificate, b"orphaned certificate").unwrap();
        let error = ensure_identity(&paths).unwrap_err();
        assert!(error.to_string().contains("identity is incomplete"));
        assert!(!paths.private_key.exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_key_is_owner_only() {
        let directory = tempfile::tempdir().unwrap();
        let paths = IdentityPaths::in_directory(directory.path());
        ensure_identity(&paths).unwrap();
        let mode = fs::metadata(paths.private_key)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn repairs_permissions_when_loading_an_existing_private_key() {
        let directory = tempfile::tempdir().unwrap();
        let paths = IdentityPaths::in_directory(directory.path());
        ensure_identity(&paths).unwrap();
        fs::set_permissions(&paths.private_key, fs::Permissions::from_mode(0o644)).unwrap();

        ensure_identity(&paths).unwrap();

        let mode = fs::metadata(paths.private_key)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
