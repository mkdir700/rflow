use std::{collections::HashSet, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScreenId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TopologyDeviceId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Edge {
    Top,
    Right,
    Bottom,
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScreenEdge {
    pub screen_id: ScreenId,
    pub edge: Edge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenNode {
    pub screen_id: ScreenId,
    pub device_id: TopologyDeviceId,
    pub device_name: String,
    pub name: String,
    pub logical_size: ScreenSize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_override: Option<ScreenSize>,
    pub online: bool,
    pub this_device: bool,
}

impl ScreenNode {
    pub fn effective_size(&self) -> ScreenSize {
        self.size_override.unwrap_or(self.logical_size)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenLink {
    pub from: ScreenEdge,
    pub to: ScreenEdge,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenTopology {
    pub revision: u64,
    pub screens: Vec<ScreenNode>,
    pub links: Vec<ScreenLink>,
}

impl ScreenTopology {
    pub fn validate(&self) -> Result<(), String> {
        let mut ids = HashSet::new();
        for screen in &self.screens {
            if screen.screen_id.0.trim().is_empty() {
                return Err("screen ID cannot be empty".to_owned());
            }
            if !ids.insert(&screen.screen_id) {
                return Err(format!("duplicate screen ID {}", screen.screen_id.0));
            }
        }
        let mut occupied = HashSet::new();
        for link in &self.links {
            if link.from == link.to {
                return Err("a screen edge cannot link to itself".to_owned());
            }
            if !ids.contains(&link.from.screen_id) || !ids.contains(&link.to.screen_id) {
                return Err("screen link references an unknown screen".to_owned());
            }
            if !occupied.insert(&link.from) || !occupied.insert(&link.to) {
                return Err("a screen edge can participate in only one link".to_owned());
            }
            if opposite(link.from.edge) != link.to.edge {
                return Err("linked screen edges must face each other".to_owned());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyRoute {
    Stay {
        screen_id: ScreenId,
        dx: i32,
        dy: i32,
    },
    Cross {
        screen_id: ScreenId,
        x: i32,
        y: i32,
    },
}

pub struct TopologyRouter<'a> {
    topology: &'a ScreenTopology,
    active: ScreenId,
    x: i32,
    y: i32,
}

impl<'a> TopologyRouter<'a> {
    pub fn new(
        topology: &'a ScreenTopology,
        active: ScreenId,
        x: i32,
        y: i32,
    ) -> Result<Self, String> {
        topology.validate()?;
        let size = topology
            .screens
            .iter()
            .find(|screen| screen.screen_id == active)
            .ok_or_else(|| format!("active screen {} is not in the topology", active.0))?
            .effective_size();
        Ok(Self {
            topology,
            active,
            x: x.clamp(0, size.width - 1),
            y: y.clamp(0, size.height - 1),
        })
    }

    pub fn active_screen(&self) -> &ScreenId {
        &self.active
    }

    pub fn route_motion(&mut self, dx: i32, dy: i32) -> TopologyRoute {
        let size = self.screen_size(&self.active);
        let next_x = self.x.saturating_add(dx);
        let next_y = self.y.saturating_add(dy);
        let crossed = if next_x < 0 {
            Some((Edge::Left, next_x, next_y))
        } else if next_x >= size.width {
            Some((Edge::Right, next_x - (size.width - 1), next_y))
        } else if next_y < 0 {
            Some((Edge::Top, next_x, next_y))
        } else if next_y >= size.height {
            Some((Edge::Bottom, next_x, next_y - (size.height - 1)))
        } else {
            None
        };
        if let Some((edge, overflow_x, overflow_y)) = crossed
            && let Some(target) = self.linked_edge(edge).cloned()
        {
            let target_size = self.screen_size(&target.screen_id);
            let (x, y) = match edge {
                Edge::Left => (
                    target_size.width.saturating_sub(2),
                    scale_axis(
                        next_y.clamp(0, size.height - 1),
                        size.height,
                        target_size.height,
                    ),
                ),
                Edge::Right => (
                    overflow_x.clamp(0, target_size.width - 1),
                    scale_axis(
                        next_y.clamp(0, size.height - 1),
                        size.height,
                        target_size.height,
                    ),
                ),
                Edge::Top => (
                    scale_axis(
                        next_x.clamp(0, size.width - 1),
                        size.width,
                        target_size.width,
                    ),
                    target_size.height.saturating_sub(2),
                ),
                Edge::Bottom => (
                    scale_axis(
                        next_x.clamp(0, size.width - 1),
                        size.width,
                        target_size.width,
                    ),
                    overflow_y.clamp(0, target_size.height - 1),
                ),
            };
            self.active = target.screen_id;
            self.x = x;
            self.y = y;
            return TopologyRoute::Cross {
                screen_id: self.active.clone(),
                x,
                y,
            };
        }
        self.x = next_x.clamp(0, size.width - 1);
        self.y = next_y.clamp(0, size.height - 1);
        TopologyRoute::Stay {
            screen_id: self.active.clone(),
            dx,
            dy,
        }
    }

    fn linked_edge(&self, edge: Edge) -> Option<&ScreenEdge> {
        let source = ScreenEdge {
            screen_id: self.active.clone(),
            edge,
        };
        self.topology.links.iter().find_map(|link| {
            if link.from == source {
                Some(&link.to)
            } else if link.to == source {
                Some(&link.from)
            } else {
                None
            }
        })
    }

    fn screen_size(&self, id: &ScreenId) -> ScreenSize {
        self.topology
            .screens
            .iter()
            .find(|screen| &screen.screen_id == id)
            .expect("validated topology contains routed screen")
            .effective_size()
    }
}

fn opposite(edge: Edge) -> Edge {
    match edge {
        Edge::Top => Edge::Bottom,
        Edge::Right => Edge::Left,
        Edge::Bottom => Edge::Top,
        Edge::Left => Edge::Right,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSizeParseError(String);

impl fmt::Display for ScreenSizeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScreenSizeParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSize {
    pub width: i32,
    pub height: i32,
}

impl ScreenSize {
    pub fn new(width: i32, height: i32) -> Result<Self, &'static str> {
        if width <= 0 || height <= 0 {
            return Err("screen dimensions must be positive");
        }
        Ok(Self { width, height })
    }
}

impl FromStr for ScreenSize {
    type Err = ScreenSizeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .split_once('x')
            .ok_or_else(|| ScreenSizeParseError("screen size must use WIDTHxHEIGHT".to_owned()))?;
        let width = width
            .parse()
            .map_err(|_| ScreenSizeParseError("invalid screen width".to_owned()))?;
        let height = height
            .parse()
            .map_err(|_| ScreenSizeParseError("invalid screen height".to_owned()))?;
        Self::new(width, height).map_err(|error| ScreenSizeParseError(error.to_owned()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenDirection {
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    TopLeft,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenDirectionParseError(String);

impl fmt::Display for ScreenDirectionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScreenDirectionParseError {}

impl fmt::Display for ScreenDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Top => "top",
            Self::TopRight => "top-right",
            Self::Right => "right",
            Self::BottomRight => "bottom-right",
            Self::Bottom => "bottom",
            Self::BottomLeft => "bottom-left",
            Self::Left => "left",
            Self::TopLeft => "top-left",
        })
    }
}

impl FromStr for ScreenDirection {
    type Err = ScreenDirectionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "top" => Ok(Self::Top),
            "top-right" => Ok(Self::TopRight),
            "right" => Ok(Self::Right),
            "bottom-right" => Ok(Self::BottomRight),
            "bottom" => Ok(Self::Bottom),
            "bottom-left" => Ok(Self::BottomLeft),
            "left" => Ok(Self::Left),
            "top-left" => Ok(Self::TopLeft),
            _ => Err(ScreenDirectionParseError(format!(
                "invalid direction {value}; expected top, top-right, right, bottom-right, bottom, bottom-left, left, or top-left"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveScreen {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    Local { dx: i32, dy: i32 },
    Remote { dx: i32, dy: i32 },
    EnterRemote { x: i32, y: i32 },
    EnterLocal { x: i32, y: i32 },
}

pub(crate) struct CursorRouter {
    local: ScreenSize,
    remote: ScreenSize,
    direction: ScreenDirection,
    active: ActiveScreen,
    x: i32,
    y: i32,
}

impl CursorRouter {
    #[cfg(test)]
    fn right(local: ScreenSize, remote: ScreenSize) -> Self {
        Self::new(local, remote, ScreenDirection::Right)
    }

    pub(crate) fn new(local: ScreenSize, remote: ScreenSize, direction: ScreenDirection) -> Self {
        Self {
            local,
            remote,
            direction,
            active: ActiveScreen::Local,
            x: local.width / 2,
            y: local.height / 2,
        }
    }

    #[cfg(test)]
    fn active(&self) -> ActiveScreen {
        self.active
    }

    pub(crate) fn set_local_position(&mut self, x: i32, y: i32) {
        self.active = ActiveScreen::Local;
        self.x = x.clamp(0, self.local.width - 1);
        self.y = y.clamp(0, self.local.height - 1);
    }

    pub(crate) fn route_motion(&mut self, dx: i32, dy: i32) -> Route {
        match self.active {
            ActiveScreen::Local => self.route_local(dx, dy),
            ActiveScreen::Remote => self.route_remote(dx, dy),
        }
    }

    fn route_local(&mut self, dx: i32, dy: i32) -> Route {
        let next_x = self.x.saturating_add(dx);
        let next_y = self.y.saturating_add(dy);
        match self.direction {
            ScreenDirection::Right if next_x >= self.local.width => {
                let overflow = next_x - (self.local.width - 1);
                self.active = ActiveScreen::Remote;
                self.x = overflow.clamp(0, self.remote.width - 1);
                self.y = scale_axis(
                    next_y.clamp(0, self.local.height - 1),
                    self.local.height,
                    self.remote.height,
                );
                Route::EnterRemote {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::Left if next_x < 0 => {
                self.active = ActiveScreen::Remote;
                self.x = self.remote.width.saturating_sub(2);
                self.y = scale_axis(
                    next_y.clamp(0, self.local.height - 1),
                    self.local.height,
                    self.remote.height,
                );
                Route::EnterRemote {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::Top if next_y < 0 => {
                self.active = ActiveScreen::Remote;
                self.x = scale_axis(
                    next_x.clamp(0, self.local.width - 1),
                    self.local.width,
                    self.remote.width,
                );
                self.y = self.remote.height.saturating_sub(2);
                Route::EnterRemote {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::TopRight if next_x >= self.local.width && next_y < 0 => {
                self.active = ActiveScreen::Remote;
                self.x = 1.min(self.remote.width - 1);
                self.y = self.remote.height.saturating_sub(2);
                Route::EnterRemote {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::Bottom if next_y >= self.local.height => {
                self.active = ActiveScreen::Remote;
                self.x = scale_axis(
                    next_x.clamp(0, self.local.width - 1),
                    self.local.width,
                    self.remote.width,
                );
                self.y = 1.min(self.remote.height - 1);
                Route::EnterRemote {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::BottomRight
                if next_x >= self.local.width && next_y >= self.local.height =>
            {
                self.active = ActiveScreen::Remote;
                self.x = 1.min(self.remote.width - 1);
                self.y = 1.min(self.remote.height - 1);
                Route::EnterRemote {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::BottomLeft if next_x < 0 && next_y >= self.local.height => {
                self.active = ActiveScreen::Remote;
                self.x = self.remote.width.saturating_sub(2);
                self.y = 1.min(self.remote.height - 1);
                Route::EnterRemote {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::TopLeft if next_x < 0 && next_y < 0 => {
                self.active = ActiveScreen::Remote;
                self.x = self.remote.width.saturating_sub(2);
                self.y = self.remote.height.saturating_sub(2);
                Route::EnterRemote {
                    x: self.x,
                    y: self.y,
                }
            }
            _ => {
                self.x = next_x.clamp(0, self.local.width - 1);
                self.y = next_y.clamp(0, self.local.height - 1);
                Route::Local { dx, dy }
            }
        }
    }

    fn route_remote(&mut self, dx: i32, dy: i32) -> Route {
        let next_x = self.x.saturating_add(dx);
        let next_y = self.y.saturating_add(dy);
        match self.direction {
            ScreenDirection::Right if next_x < 0 => {
                self.active = ActiveScreen::Local;
                self.x = self.local.width.saturating_sub(2);
                self.y = scale_axis(
                    next_y.clamp(0, self.remote.height - 1),
                    self.remote.height,
                    self.local.height,
                );
                Route::EnterLocal {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::Left if next_x >= self.remote.width => {
                self.active = ActiveScreen::Local;
                self.x = 1.min(self.local.width - 1);
                self.y = scale_axis(
                    next_y.clamp(0, self.remote.height - 1),
                    self.remote.height,
                    self.local.height,
                );
                Route::EnterLocal {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::Top if next_y >= self.remote.height => {
                self.active = ActiveScreen::Local;
                self.x = scale_axis(
                    next_x.clamp(0, self.remote.width - 1),
                    self.remote.width,
                    self.local.width,
                );
                self.y = 1.min(self.local.height - 1);
                Route::EnterLocal {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::TopRight if next_x < 0 && next_y >= self.remote.height => {
                self.active = ActiveScreen::Local;
                self.x = self.local.width.saturating_sub(2);
                self.y = 1.min(self.local.height - 1);
                Route::EnterLocal {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::Bottom if next_y < 0 => {
                self.active = ActiveScreen::Local;
                self.x = scale_axis(
                    next_x.clamp(0, self.remote.width - 1),
                    self.remote.width,
                    self.local.width,
                );
                self.y = self.local.height.saturating_sub(2);
                Route::EnterLocal {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::BottomRight if next_x < 0 && next_y < 0 => {
                self.active = ActiveScreen::Local;
                self.x = self.local.width.saturating_sub(2);
                self.y = self.local.height.saturating_sub(2);
                Route::EnterLocal {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::BottomLeft if next_x >= self.remote.width && next_y < 0 => {
                self.active = ActiveScreen::Local;
                self.x = 1.min(self.local.width - 1);
                self.y = self.local.height.saturating_sub(2);
                Route::EnterLocal {
                    x: self.x,
                    y: self.y,
                }
            }
            ScreenDirection::TopLeft
                if next_x >= self.remote.width && next_y >= self.remote.height =>
            {
                self.active = ActiveScreen::Local;
                self.x = 1.min(self.local.width - 1);
                self.y = 1.min(self.local.height - 1);
                Route::EnterLocal {
                    x: self.x,
                    y: self.y,
                }
            }
            _ => {
                self.x = next_x.clamp(0, self.remote.width - 1);
                self.y = next_y.clamp(0, self.remote.height - 1);
                Route::Remote { dx, dy }
            }
        }
    }
}

fn scale_axis(value: i32, from: i32, to: i32) -> i32 {
    ((i64::from(value) * i64::from(to)) / i64::from(from)).clamp(0, i64::from(to - 1)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> ScreenNode {
        ScreenNode {
            screen_id: ScreenId(id.to_owned()),
            device_id: TopologyDeviceId(format!("device-{id}")),
            device_name: format!("device-{id}"),
            name: "display-1".to_owned(),
            logical_size: ScreenSize::new(100, 100).unwrap(),
            size_override: None,
            online: true,
            this_device: id == "a",
        }
    }

    fn router() -> CursorRouter {
        CursorRouter::right(
            ScreenSize::new(1920, 1080).unwrap(),
            ScreenSize::new(2560, 1440).unwrap(),
        )
    }

    #[test]
    fn crossing_host_right_edge_enters_client_at_scaled_height() {
        let mut router = router();
        router.set_local_position(1918, 540);

        assert_eq!(
            router.route_motion(5, 0),
            Route::EnterRemote { x: 4, y: 720 }
        );
        assert_eq!(router.active(), ActiveScreen::Remote);
    }

    #[test]
    fn left_layout_crosses_host_left_edge_and_returns_at_remote_right_edge() {
        let mut router = CursorRouter::new(
            ScreenSize::new(100, 100).unwrap(),
            ScreenSize::new(200, 200).unwrap(),
            ScreenDirection::Left,
        );
        router.set_local_position(1, 25);

        assert_eq!(
            router.route_motion(-3, 0),
            Route::EnterRemote { x: 198, y: 50 }
        );
        assert_eq!(router.route_motion(3, 0), Route::EnterLocal { x: 1, y: 25 });
    }

    #[test]
    fn top_layout_crosses_host_top_edge_and_returns_at_remote_bottom_edge() {
        let mut router = CursorRouter::new(
            ScreenSize::new(100, 100).unwrap(),
            ScreenSize::new(200, 200).unwrap(),
            ScreenDirection::Top,
        );
        router.set_local_position(25, 1);

        assert_eq!(
            router.route_motion(0, -3),
            Route::EnterRemote { x: 50, y: 198 }
        );
        assert_eq!(router.route_motion(0, 3), Route::EnterLocal { x: 25, y: 1 });
    }

    #[test]
    fn top_right_layout_requires_corner_crossing_in_both_directions() {
        let mut router = CursorRouter::new(
            ScreenSize::new(100, 100).unwrap(),
            ScreenSize::new(200, 200).unwrap(),
            ScreenDirection::TopRight,
        );
        router.set_local_position(98, 1);

        assert_eq!(router.route_motion(3, 0), Route::Local { dx: 3, dy: 0 });
        assert_eq!(
            router.route_motion(3, -3),
            Route::EnterRemote { x: 1, y: 198 }
        );
        assert_eq!(
            router.route_motion(-3, 3),
            Route::EnterLocal { x: 98, y: 1 }
        );
    }

    #[test]
    fn bottom_and_remaining_corner_layouts_cross_at_their_matching_edges() {
        let cases = [
            (
                ScreenDirection::Bottom,
                (25, 98),
                (0, 3),
                Route::EnterRemote { x: 50, y: 1 },
                (0, -3),
                Route::EnterLocal { x: 25, y: 98 },
            ),
            (
                ScreenDirection::BottomRight,
                (98, 98),
                (3, 3),
                Route::EnterRemote { x: 1, y: 1 },
                (-3, -3),
                Route::EnterLocal { x: 98, y: 98 },
            ),
            (
                ScreenDirection::BottomLeft,
                (1, 98),
                (-3, 3),
                Route::EnterRemote { x: 198, y: 1 },
                (3, -3),
                Route::EnterLocal { x: 1, y: 98 },
            ),
            (
                ScreenDirection::TopLeft,
                (1, 1),
                (-3, -3),
                Route::EnterRemote { x: 198, y: 198 },
                (3, 3),
                Route::EnterLocal { x: 1, y: 1 },
            ),
        ];
        for (direction, start, outbound, entered, inbound, returned) in cases {
            let mut router = CursorRouter::new(
                ScreenSize::new(100, 100).unwrap(),
                ScreenSize::new(200, 200).unwrap(),
                direction,
            );
            router.set_local_position(start.0, start.1);
            assert_eq!(router.route_motion(outbound.0, outbound.1), entered);
            assert_eq!(router.route_motion(inbound.0, inbound.1), returned);
        }
    }

    #[test]
    fn crossing_client_left_edge_returns_to_host() {
        let mut router = router();
        router.set_local_position(1918, 540);
        router.route_motion(5, 0);

        assert_eq!(
            router.route_motion(-5, 0),
            Route::EnterLocal { x: 1918, y: 540 }
        );
        assert_eq!(router.active(), ActiveScreen::Local);
    }

    #[test]
    fn motion_stays_on_the_active_screen_before_an_edge() {
        let mut router = router();
        assert_eq!(router.route_motion(10, -4), Route::Local { dx: 10, dy: -4 });
        router.set_local_position(1918, 540);
        router.route_motion(5, 0);
        assert_eq!(router.route_motion(8, 3), Route::Remote { dx: 8, dy: 3 });
    }

    #[test]
    fn screen_dimensions_must_be_positive() {
        assert!(ScreenSize::new(0, 1080).is_err());
        assert!(ScreenSize::new(1920, -1).is_err());
    }

    #[test]
    fn screen_size_parses_cli_notation() {
        assert_eq!(
            "2560x1440".parse::<ScreenSize>().unwrap(),
            ScreenSize {
                width: 2560,
                height: 1440
            }
        );
        assert!("2560".parse::<ScreenSize>().is_err());
        assert!("0x1440".parse::<ScreenSize>().is_err());
    }

    #[test]
    fn topology_supports_three_devices_and_cycles() {
        let topology = ScreenTopology {
            revision: 3,
            screens: vec![node("a"), node("b"), node("c")],
            links: vec![
                ScreenLink {
                    from: ScreenEdge {
                        screen_id: ScreenId("a".into()),
                        edge: Edge::Right,
                    },
                    to: ScreenEdge {
                        screen_id: ScreenId("b".into()),
                        edge: Edge::Left,
                    },
                },
                ScreenLink {
                    from: ScreenEdge {
                        screen_id: ScreenId("b".into()),
                        edge: Edge::Bottom,
                    },
                    to: ScreenEdge {
                        screen_id: ScreenId("c".into()),
                        edge: Edge::Top,
                    },
                },
                ScreenLink {
                    from: ScreenEdge {
                        screen_id: ScreenId("c".into()),
                        edge: Edge::Left,
                    },
                    to: ScreenEdge {
                        screen_id: ScreenId("a".into()),
                        edge: Edge::Right,
                    },
                },
            ],
        };
        assert!(
            topology.validate().is_err(),
            "a.right is intentionally conflicting"
        );

        let mut valid = topology;
        valid.links.pop();
        assert_eq!(valid.validate(), Ok(()));
    }

    #[test]
    fn topology_rejects_unknown_endpoints_and_non_facing_edges() {
        let mut topology = ScreenTopology {
            revision: 1,
            screens: vec![node("a"), node("b")],
            links: vec![ScreenLink {
                from: ScreenEdge {
                    screen_id: ScreenId("a".into()),
                    edge: Edge::Right,
                },
                to: ScreenEdge {
                    screen_id: ScreenId("missing".into()),
                    edge: Edge::Left,
                },
            }],
        };
        assert!(topology.validate().unwrap_err().contains("unknown"));
        topology.links[0].to.screen_id = ScreenId("b".into());
        topology.links[0].to.edge = Edge::Top;
        assert!(topology.validate().unwrap_err().contains("face"));
    }

    #[test]
    fn topology_router_crosses_a_three_screen_chain_by_named_links() {
        let topology = ScreenTopology {
            revision: 1,
            screens: vec![node("a"), node("b"), node("c")],
            links: vec![
                ScreenLink {
                    from: ScreenEdge {
                        screen_id: ScreenId("a".into()),
                        edge: Edge::Right,
                    },
                    to: ScreenEdge {
                        screen_id: ScreenId("b".into()),
                        edge: Edge::Left,
                    },
                },
                ScreenLink {
                    from: ScreenEdge {
                        screen_id: ScreenId("b".into()),
                        edge: Edge::Right,
                    },
                    to: ScreenEdge {
                        screen_id: ScreenId("c".into()),
                        edge: Edge::Left,
                    },
                },
            ],
        };
        let mut router = TopologyRouter::new(&topology, ScreenId("a".into()), 98, 50).unwrap();
        assert!(
            matches!(router.route_motion(4, 0), TopologyRoute::Cross { screen_id: ScreenId(ref id), .. } if id == "b")
        );
        assert!(
            matches!(router.route_motion(100, 0), TopologyRoute::Cross { screen_id: ScreenId(ref id), .. } if id == "c")
        );
        assert_eq!(router.active_screen(), &ScreenId("c".into()));
    }
}
