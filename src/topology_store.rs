use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result, bail};

use crate::{
    core::{
        LayoutCommand, ScreenDescriptor, ScreenInventory, ScreenLayout, ScreenSizeOverride,
        ScreenTopology,
    },
    identity::application_config_directory,
};

pub const TOPOLOGY_FILE: &str = "topology.json";
pub const INVENTORY_FILE: &str = "screen-inventory.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyState {
    pub layout: ScreenLayout,
    pub inventory: ScreenInventory,
}

pub fn default_path() -> Result<PathBuf> {
    Ok(application_config_directory()?.join(TOPOLOGY_FILE))
}

pub fn load(path: &Path) -> Result<Option<ScreenTopology>> {
    if !path.exists() && !inventory_path(path)?.exists() {
        return Ok(None);
    }
    let state = load_state(path)?;
    Ok(Some(
        state
            .layout
            .resolve(&state.inventory)
            .map_err(anyhow::Error::msg)?,
    ))
}

pub fn load_state(path: &Path) -> Result<TopologyState> {
    let inventory_file = inventory_path(path)?;
    let cached_inventory =
        read_optional::<ScreenInventory>(&inventory_file, "screen inventory")?.unwrap_or_default();
    let Some(bytes) = read_optional_bytes(path, "screen topology")? else {
        return Ok(TopologyState {
            layout: ScreenLayout::default(),
            inventory: cached_inventory,
        });
    };
    if let Ok(layout) = serde_json::from_slice::<ScreenLayout>(&bytes) {
        layout
            .validate(&cached_inventory)
            .map_err(anyhow::Error::msg)?;
        return Ok(TopologyState {
            layout,
            inventory: cached_inventory,
        });
    }
    let legacy: ScreenTopology =
        serde_json::from_slice(&bytes).context("decode screen topology")?;
    legacy.validate().map_err(anyhow::Error::msg)?;
    let inventory = ScreenInventory {
        screens: legacy
            .screens
            .iter()
            .map(|screen| ScreenDescriptor {
                screen_id: screen.screen_id.clone(),
                device_id: screen.device_id.clone(),
                device_name: screen.device_name.clone(),
                name: screen.name.clone(),
                logical_size: screen.logical_size,
                primary: screen.this_device,
                online: screen.online,
                this_device: screen.this_device,
            })
            .collect(),
    };
    let layout = ScreenLayout {
        revision: legacy.revision,
        links: legacy.links,
        size_overrides: legacy
            .screens
            .into_iter()
            .filter_map(|screen| {
                screen.size_override.map(|size| ScreenSizeOverride {
                    screen_id: screen.screen_id,
                    size,
                })
            })
            .collect(),
        ..ScreenLayout::default()
    };
    layout.validate(&inventory).map_err(anyhow::Error::msg)?;
    Ok(TopologyState { layout, inventory })
}

pub fn save_inventory(path: &Path, inventory: &ScreenInventory) -> Result<()> {
    inventory.validate().map_err(anyhow::Error::msg)?;
    write_json_atomic(&inventory_path(path)?, inventory, "screen inventory")
}

pub fn apply(
    path: &Path,
    expected_revision: u64,
    inventory: &ScreenInventory,
    command: LayoutCommand,
) -> Result<ScreenTopology> {
    let state = load_state(path)?;
    if state.layout.revision != expected_revision {
        bail!(
            "stale topology revision: expected {expected_revision}, current is {}",
            state.layout.revision
        );
    }
    let layout = state
        .layout
        .apply(inventory, command)
        .map_err(anyhow::Error::msg)?;
    save_inventory(path, inventory)?;
    write_json_atomic(path, &layout, "screen topology")?;
    layout.resolve(inventory).map_err(anyhow::Error::msg)
}

pub fn replace(
    path: &Path,
    expected_revision: u64,
    mut topology: ScreenTopology,
) -> Result<ScreenTopology> {
    topology.validate().map_err(anyhow::Error::msg)?;
    let current_revision = load_state(path)?.layout.revision;
    if current_revision != expected_revision {
        bail!(
            "stale topology revision: expected {expected_revision}, current is {current_revision}"
        );
    }
    topology.revision = current_revision.saturating_add(1);
    let inventory = ScreenInventory {
        screens: topology
            .screens
            .iter()
            .map(|screen| ScreenDescriptor {
                screen_id: screen.screen_id.clone(),
                device_id: screen.device_id.clone(),
                device_name: screen.device_name.clone(),
                name: screen.name.clone(),
                logical_size: screen.logical_size,
                primary: screen.this_device,
                online: screen.online,
                this_device: screen.this_device,
            })
            .collect(),
    };
    let layout = ScreenLayout {
        revision: topology.revision,
        links: topology.links.clone(),
        size_overrides: topology
            .screens
            .iter()
            .filter_map(|screen| {
                screen.size_override.map(|size| ScreenSizeOverride {
                    screen_id: screen.screen_id.clone(),
                    size,
                })
            })
            .collect(),
        ..ScreenLayout::default()
    };
    layout.validate(&inventory).map_err(anyhow::Error::msg)?;
    save_inventory(path, &inventory)?;
    write_json_atomic(path, &layout, "screen topology")?;
    Ok(topology)
}

fn inventory_path(path: &Path) -> Result<PathBuf> {
    Ok(path
        .parent()
        .context("topology path has no parent directory")?
        .join(INVENTORY_FILE))
}

fn read_optional<T: serde::de::DeserializeOwned>(
    path: &Path,
    description: &str,
) -> Result<Option<T>> {
    let Some(bytes) = read_optional_bytes(path, description)? else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_slice(&bytes).with_context(|| format!("decode {description}"))?,
    ))
}

fn read_optional_bytes(path: &Path, description: &str) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {description}")),
    }
}

fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T, description: &str) -> Result<()> {
    let directory = path
        .parent()
        .with_context(|| format!("{description} path has no parent directory"))?;
    fs::create_dir_all(directory).with_context(|| format!("create {description} directory"))?;
    #[cfg(unix)]
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .context("secure topology directory")?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let bytes =
        serde_json::to_vec_pretty(value).with_context(|| format!("encode {description}"))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create temporary {description}"))?;
    file.write_all(&bytes)
        .with_context(|| format!("write temporary {description}"))?;
    file.sync_all()
        .with_context(|| format!("sync temporary {description}"))?;
    fs::rename(&temporary, path).with_context(|| format!("install {description}"))?;
    #[cfg(unix)]
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync {description} directory"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        Edge, LayoutCommand, ScreenDescriptor, ScreenEdge, ScreenId, ScreenInventory, ScreenSize,
        TopologyDeviceId,
    };

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

    #[test]
    fn layout_commands_persist_only_user_intent_and_increment_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(TOPOLOGY_FILE);
        let inventory = ScreenInventory {
            screens: vec![descriptor("local", true), descriptor("remote", true)],
        };

        save_inventory(&path, &inventory).unwrap();
        let topology = apply(
            &path,
            0,
            &inventory,
            LayoutCommand::Link {
                from: ScreenEdge {
                    screen_id: ScreenId("local".into()),
                    edge: Edge::Right,
                },
                to: ScreenEdge {
                    screen_id: ScreenId("remote".into()),
                    edge: Edge::Left,
                },
                replace: false,
            },
        )
        .unwrap();

        assert_eq!(topology.revision, 1);
        let persisted = fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("schema_version"));
        assert!(!persisted.contains("online"));
        assert!(!persisted.contains("device_name"));
        assert!(
            apply(
                &path,
                0,
                &inventory,
                LayoutCommand::Unplace {
                    screen_id: ScreenId("remote".into())
                },
            )
            .unwrap_err()
            .to_string()
            .contains("stale")
        );
    }

    #[test]
    fn legacy_topology_is_loaded_as_layout_and_inventory_without_data_loss() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(TOPOLOGY_FILE);
        let legacy = ScreenTopology {
            revision: 7,
            screens: vec![crate::core::ScreenNode {
                screen_id: ScreenId("local".into()),
                device_id: TopologyDeviceId("device".into()),
                device_name: "linux".into(),
                name: "DP-1".into(),
                logical_size: ScreenSize::new(100, 100).unwrap(),
                size_override: Some(ScreenSize::new(200, 200).unwrap()),
                online: false,
                this_device: true,
            }],
            links: Vec::new(),
        };
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let state = load_state(&path).unwrap();
        assert_eq!(state.layout.revision, 7);
        assert_eq!(
            state.layout.size_overrides[0].size,
            ScreenSize::new(200, 200).unwrap()
        );
        assert_eq!(state.inventory.screens[0].name, "DP-1");
    }

    fn descriptor(id: &str, online: bool) -> ScreenDescriptor {
        ScreenDescriptor {
            screen_id: ScreenId(id.into()),
            device_id: TopologyDeviceId(format!("device-{id}")),
            device_name: format!("device-{id}"),
            name: format!("display-{id}"),
            logical_size: ScreenSize::new(100, 100).unwrap(),
            primary: id == "local",
            online,
            this_device: id == "local",
        }
    }
}
