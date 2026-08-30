use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .split_once('x')
            .ok_or_else(|| "screen size must use WIDTHxHEIGHT".to_owned())?;
        let width = width
            .parse()
            .map_err(|_| "invalid screen width".to_owned())?;
        let height = height
            .parse()
            .map_err(|_| "invalid screen height".to_owned())?;
        Self::new(width, height).map_err(str::to_owned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveScreen {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Local { dx: i32, dy: i32 },
    Remote { dx: i32, dy: i32 },
    EnterRemote { x: i32, y: i32 },
    EnterLocal { x: i32, y: i32 },
}

pub struct CursorRouter {
    local: ScreenSize,
    remote: ScreenSize,
    active: ActiveScreen,
    x: i32,
    y: i32,
}

impl CursorRouter {
    pub fn right(local: ScreenSize, remote: ScreenSize) -> Self {
        Self {
            local,
            remote,
            active: ActiveScreen::Local,
            x: local.width / 2,
            y: local.height / 2,
        }
    }

    pub fn active(&self) -> ActiveScreen {
        self.active
    }

    pub fn set_local_position(&mut self, x: i32, y: i32) {
        self.active = ActiveScreen::Local;
        self.x = x.clamp(0, self.local.width - 1);
        self.y = y.clamp(0, self.local.height - 1);
    }

    pub fn route_motion(&mut self, dx: i32, dy: i32) -> Route {
        match self.active {
            ActiveScreen::Local => self.route_local(dx, dy),
            ActiveScreen::Remote => self.route_remote(dx, dy),
        }
    }

    fn route_local(&mut self, dx: i32, dy: i32) -> Route {
        let next_x = self.x.saturating_add(dx);
        self.y = self.y.saturating_add(dy).clamp(0, self.local.height - 1);
        if next_x >= self.local.width {
            let overflow = next_x - (self.local.width - 1);
            self.active = ActiveScreen::Remote;
            self.x = overflow.clamp(0, self.remote.width - 1);
            self.y = scale_axis(self.y, self.local.height, self.remote.height);
            Route::EnterRemote {
                x: self.x,
                y: self.y,
            }
        } else {
            self.x = next_x.clamp(0, self.local.width - 1);
            Route::Local { dx, dy }
        }
    }

    fn route_remote(&mut self, dx: i32, dy: i32) -> Route {
        let next_x = self.x.saturating_add(dx);
        self.y = self.y.saturating_add(dy).clamp(0, self.remote.height - 1);
        if next_x < 0 {
            self.active = ActiveScreen::Local;
            self.x = self.local.width.saturating_sub(2);
            self.y = scale_axis(self.y, self.remote.height, self.local.height);
            Route::EnterLocal {
                x: self.x,
                y: self.y,
            }
        } else {
            self.x = next_x.clamp(0, self.remote.width - 1);
            Route::Remote { dx, dy }
        }
    }
}

fn scale_axis(value: i32, from: i32, to: i32) -> i32 {
    ((i64::from(value) * i64::from(to)) / i64::from(from)).clamp(0, i64::from(to - 1)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
