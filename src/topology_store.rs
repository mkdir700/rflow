use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result, bail};

use crate::{core::ScreenTopology, identity::application_config_directory};

pub const TOPOLOGY_FILE: &str = "topology.json";

pub fn default_path() -> Result<PathBuf> {
    Ok(application_config_directory()?.join(TOPOLOGY_FILE))
}

pub fn load(path: &Path) -> Result<Option<ScreenTopology>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read screen topology"),
    };
    let topology: ScreenTopology =
        serde_json::from_slice(&bytes).context("decode screen topology")?;
    topology.validate().map_err(anyhow::Error::msg)?;
    Ok(Some(topology))
}

pub fn replace(
    path: &Path,
    expected_revision: u64,
    mut topology: ScreenTopology,
) -> Result<ScreenTopology> {
    topology.validate().map_err(anyhow::Error::msg)?;
    let current_revision = load(path)?.map_or(0, |current| current.revision);
    if current_revision != expected_revision {
        bail!(
            "stale topology revision: expected {expected_revision}, current is {current_revision}"
        );
    }
    topology.revision = current_revision.saturating_add(1);
    let directory = path
        .parent()
        .context("topology path has no parent directory")?;
    fs::create_dir_all(directory).context("create topology directory")?;
    #[cfg(unix)]
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .context("secure topology directory")?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(&topology).context("encode screen topology")?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .context("create temporary screen topology")?;
    file.write_all(&bytes)
        .context("write temporary screen topology")?;
    file.sync_all().context("sync temporary screen topology")?;
    fs::rename(&temporary, path).context("install screen topology")?;
    #[cfg(unix)]
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .context("sync topology directory")?;
    Ok(topology)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_is_atomic_and_revision_guarded() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(TOPOLOGY_FILE);
        let saved = replace(&path, 0, ScreenTopology::default()).unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(load(&path).unwrap(), Some(saved));
        assert!(
            replace(&path, 0, ScreenTopology::default())
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
    }
}
