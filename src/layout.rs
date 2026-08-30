use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;

use crate::core::{ScreenLink, ScreenNode, ScreenTopology};

#[derive(Serialize)]
struct LayoutDocument<'a> {
    schema_version: u16,
    revision: u64,
    screens: &'a [ScreenNode],
    links: &'a [ScreenLink],
}

pub fn render_json(topology: &ScreenTopology) -> Result<String> {
    Ok(serde_json::to_string_pretty(&LayoutDocument {
        schema_version: 1,
        revision: topology.revision,
        screens: &topology.screens,
        links: &topology.links,
    })?)
}

pub fn render_text(topology: &ScreenTopology) -> String {
    if topology.screens.is_empty() {
        return "Screen layout\n\nNo screens are available.".to_owned();
    }
    let mut devices: BTreeMap<&str, Vec<&ScreenNode>> = BTreeMap::new();
    for screen in &topology.screens {
        devices.entry(&screen.device_name).or_default().push(screen);
    }
    let mut output = String::from("Screen layout\n");
    for (device, screens) in devices {
        let local = screens.iter().any(|screen| screen.this_device);
        output.push_str(&format!("\n{}{}\n", device, if local { " ★" } else { "" }));
        for screen in screens {
            output.push_str(&format!(
                "  {}  {}×{}  {}\n",
                screen.name,
                screen.logical_size.width,
                screen.logical_size.height,
                if screen.online { "online" } else { "offline" }
            ));
        }
    }
    output.push_str("\nLinks\n");
    if topology.links.is_empty() {
        output.push_str("  No screen links configured.\n");
    } else {
        for link in &topology.links {
            output.push_str(&format!(
                "  {}.{:?} ↔ {}.{:?}\n",
                link.from.screen_id.0, link.from.edge, link.to.screen_id.0, link.to.edge
            ));
        }
    }
    output.push_str("\n★ This device");
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ScreenId, ScreenSize, TopologyDeviceId};

    fn topology() -> ScreenTopology {
        ScreenTopology {
            revision: 7,
            screens: vec![ScreenNode {
                screen_id: ScreenId("display-1".into()),
                device_id: TopologyDeviceId("device-1".into()),
                device_name: "linux-desktop".into(),
                name: "DP-1".into(),
                logical_size: ScreenSize::new(2560, 1440).unwrap(),
                online: true,
                this_device: true,
            }],
            links: Vec::new(),
        }
    }

    #[test]
    fn text_marks_local_device_and_dimensions() {
        let rendered = render_text(&topology());
        assert!(rendered.contains("linux-desktop ★"));
        assert!(rendered.contains("DP-1  2560×1440"));
    }

    #[test]
    fn json_has_an_explicit_schema_version() {
        let value: serde_json::Value =
            serde_json::from_str(&render_json(&topology()).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["revision"], 7);
    }
}
