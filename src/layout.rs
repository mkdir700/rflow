use std::collections::{HashMap, VecDeque};

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
    let (diagram, complete) = render_diagram(topology);
    let mut output = format!("Screen layout\n\n{diagram}\n");
    if !complete {
        output.push_str("\nLayout contains disconnected nodes or conflicting coordinates.\n");
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

fn render_diagram(topology: &ScreenTopology) -> (String, bool) {
    const BOX_WIDTH: usize = 28;
    const BOX_HEIGHT: usize = 4;
    let mut positions: HashMap<&str, (i32, i32)> = HashMap::new();
    let mut queue = VecDeque::new();
    let first = topology
        .screens
        .iter()
        .find(|screen| screen.this_device)
        .unwrap_or(&topology.screens[0]);
    positions.insert(&first.screen_id.0, (0, 0));
    queue.push_back(first.screen_id.0.as_str());
    let mut conflict = false;
    while let Some(id) = queue.pop_front() {
        let origin = positions[id];
        for link in &topology.links {
            let (target, edge) = if link.from.screen_id.0 == *id {
                (&link.to.screen_id.0, link.from.edge)
            } else if link.to.screen_id.0 == *id {
                (&link.from.screen_id.0, link.to.edge)
            } else {
                continue;
            };
            let delta = match edge {
                crate::core::Edge::Top => (0, -1),
                crate::core::Edge::Right => (1, 0),
                crate::core::Edge::Bottom => (0, 1),
                crate::core::Edge::Left => (-1, 0),
            };
            let proposed = (origin.0 + delta.0, origin.1 + delta.1);
            match positions.get(target.as_str()) {
                Some(existing) if *existing != proposed => conflict = true,
                Some(_) => {}
                None => {
                    positions.insert(target, proposed);
                    queue.push_back(target.as_str());
                }
            }
        }
    }
    let connected = positions.len();
    let mut next_x = positions
        .values()
        .map(|position| position.0)
        .max()
        .unwrap_or(-1)
        + 2;
    for screen in &topology.screens {
        if !positions.contains_key(screen.screen_id.0.as_str()) {
            positions.insert(&screen.screen_id.0, (next_x, 0));
            next_x += 1;
        }
    }
    let min_x = positions.values().map(|p| p.0).min().unwrap_or(0);
    let min_y = positions.values().map(|p| p.1).min().unwrap_or(0);
    let max_x = positions.values().map(|p| p.0).max().unwrap_or(0);
    let max_y = positions.values().map(|p| p.1).max().unwrap_or(0);
    let cell_width = BOX_WIDTH + 3;
    let cell_height = BOX_HEIGHT + 2;
    let mut canvas = vec![
        vec![' '; (max_x - min_x + 1) as usize * cell_width];
        (max_y - min_y + 1) as usize * cell_height
    ];
    for screen in &topology.screens {
        let (gx, gy) = positions[screen.screen_id.0.as_str()];
        draw_box(
            &mut canvas,
            (gx - min_x) as usize * cell_width,
            (gy - min_y) as usize * cell_height,
            screen,
        );
    }
    let diagram = canvas
        .into_iter()
        .map(|line| line.into_iter().collect::<String>().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    (
        diagram.trim_end().to_owned(),
        connected == topology.screens.len() && !conflict,
    )
}

fn draw_box(canvas: &mut [Vec<char>], x: usize, y: usize, screen: &ScreenNode) {
    const WIDTH: usize = 28;
    canvas[y][x + 1..x + WIDTH - 1].fill('─');
    canvas[y + 3][x + 1..x + WIDTH - 1].fill('─');
    canvas[y][x] = '┌';
    canvas[y][x + WIDTH - 1] = '┐';
    canvas[y + 3][x] = '└';
    canvas[y + 3][x + WIDTH - 1] = '┘';
    canvas[y + 1][x] = '│';
    canvas[y + 1][x + WIDTH - 1] = '│';
    canvas[y + 2][x] = '│';
    canvas[y + 2][x + WIDTH - 1] = '│';
    let size = screen.effective_size();
    write_clipped(
        canvas,
        x + 2,
        y + 1,
        &format!(
            "{}{}",
            screen.device_name,
            if screen.this_device { " ★" } else { "" }
        ),
        WIDTH - 4,
    );
    write_clipped(
        canvas,
        x + 2,
        y + 2,
        &format!(
            "{} {}×{}{}",
            screen.name,
            size.width,
            size.height,
            if screen.online { "" } else { " offline" }
        ),
        WIDTH - 4,
    );
}

fn write_clipped(canvas: &mut [Vec<char>], x: usize, y: usize, text: &str, limit: usize) {
    for (offset, character) in text.chars().take(limit).enumerate() {
        canvas[y][x + offset] = character;
    }
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
                size_override: None,
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
        assert!(rendered.contains("DP-1 2560×1440"));
    }

    #[test]
    fn json_has_an_explicit_schema_version() {
        let value: serde_json::Value =
            serde_json::from_str(&render_json(&topology()).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["revision"], 7);
    }
}
