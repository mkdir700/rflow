use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use tokio::sync::{mpsc, watch};

use crate::{
    core::{Button, ButtonState, InputEvent, Key, Motion, ScreenSize},
    input,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturedEvent {
    Input { sequence: u64, event: InputEvent },
    Motion(Motion),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetectedScreen {
    pub stable_id: String,
    pub name: String,
    pub logical_size: ScreenSize,
    pub scale: f64,
    pub x: i32,
    pub y: i32,
    pub primary: bool,
}

/// Owns a platform capture session and hides its channel/protocol plumbing.
pub struct InputCapture {
    reliable: mpsc::Receiver<CapturedEvent>,
    motion: watch::Receiver<Option<Motion>>,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for InputCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.reliable.close();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl InputCapture {
    pub async fn next(&mut self) -> Result<CapturedEvent> {
        loop {
            tokio::select! {
                reliable = self.reliable.recv() => {
                    return reliable.context("all input capture threads stopped");
                }
                changed = self.motion.changed() => {
                    changed.context("all pointer capture threads stopped")?;
                    if let Some(motion) = *self.motion.borrow_and_update() {
                        return Ok(CapturedEvent::Motion(motion));
                    }
                }
            }
        }
    }
}

pub fn validate_capture(paths: &[PathBuf]) -> Result<()> {
    input::validate_capture(paths)
}

pub fn resolve_capture_devices(overrides: &[PathBuf]) -> Result<Vec<PathBuf>> {
    input::resolve_capture_devices(overrides)
}

pub fn capture(paths: Vec<PathBuf>, grab: bool) -> Result<InputCapture> {
    let (reliable_tx, reliable) = mpsc::channel(256);
    let (motion_tx, motion) = watch::channel(None);
    let stop = Arc::new(AtomicBool::new(false));
    let callback: input::CaptureCallback = Arc::new(move |captured| match captured {
        input::NativeCapturedEvent::Input {
            sequence,
            event_type,
            code,
            value,
        } => match decode_native_input(event_type, code, value) {
            Ok(event) => {
                let _ = reliable_tx.blocking_send(CapturedEvent::Input { sequence, event });
            }
            Err(error) => tracing::debug!(%error, event_type, code, "ignore native input"),
        },
        input::NativeCapturedEvent::Motion {
            sequence,
            timestamp_micros,
            dx,
            dy,
        } => {
            motion_tx.send_replace(Some(Motion {
                sequence,
                timestamp_micros,
                dx,
                dy,
            }));
        }
    });
    let threads = input::spawn_capture(paths, grab, callback, stop.clone())?;
    Ok(InputCapture {
        reliable,
        motion,
        stop,
        threads,
    })
}

pub fn screen_size() -> Result<ScreenSize> {
    let screens = screens()?;
    screens
        .iter()
        .find(|screen| screen.primary)
        .or_else(|| screens.first())
        .map(|screen| screen.logical_size)
        .context("platform reported no active screens")
}

pub fn screens() -> Result<Vec<DetectedScreen>> {
    input::screens()
}

pub fn cursor_position() -> Option<(i32, i32)> {
    input::cursor_position()
}

pub struct InputInjector {
    inner: input::Injector,
}

impl InputInjector {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: input::Injector::new()?,
        })
    }

    pub fn emit(&mut self, event: InputEvent) -> Result<()> {
        for (event_type, code, value) in encode_native_input(event) {
            self.inner.emit_raw(event_type, code, value)?;
        }
        Ok(())
    }

    pub fn emit_motion(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.inner.emit_motion(dx, dy)
    }

    pub fn set_cursor_position(&mut self, x: i32, y: i32) -> Result<()> {
        self.inner.set_cursor_position(x, y)
    }
}

fn decode_native_input(event_type: u16, code: u16, value: i32) -> Result<InputEvent> {
    match (event_type, code) {
        (1, code @ 272..=287) => Ok(InputEvent::Button {
            button: decode_button(code),
            state: decode_state(value),
        }),
        (1, code) => Ok(InputEvent::Key {
            key: Key(code),
            state: decode_state(value),
        }),
        (2, 6) => Ok(InputEvent::Scroll {
            horizontal: value,
            vertical: 0,
        }),
        (2, 8) => Ok(InputEvent::Scroll {
            horizontal: 0,
            vertical: value,
        }),
        _ => bail!("unsupported native input type={event_type} code={code}"),
    }
}

fn encode_native_input(event: InputEvent) -> Vec<(u16, u16, i32)> {
    match event {
        InputEvent::Key { key, state } => vec![(1, key.0, encode_state(state))],
        InputEvent::Button { button, state } => {
            vec![(1, encode_button(button), encode_state(state))]
        }
        InputEvent::Scroll {
            horizontal,
            vertical,
        } => {
            let mut events = Vec::with_capacity(2);
            if horizontal != 0 {
                events.push((2, 6, horizontal));
            }
            if vertical != 0 {
                events.push((2, 8, vertical));
            }
            events
        }
    }
}

fn decode_state(value: i32) -> ButtonState {
    match value {
        0 => ButtonState::Released,
        2 => ButtonState::Repeated,
        _ => ButtonState::Pressed,
    }
}

fn encode_state(state: ButtonState) -> i32 {
    match state {
        ButtonState::Pressed => 1,
        ButtonState::Released => 0,
        ButtonState::Repeated => 2,
    }
}

fn decode_button(code: u16) -> Button {
    match code {
        272 => Button::Left,
        273 => Button::Right,
        274 => Button::Middle,
        275 => Button::Back,
        276 => Button::Forward,
        code => Button::Other(code),
    }
}

fn encode_button(button: Button) -> u16 {
    match button {
        Button::Left => 272,
        Button::Right => 273,
        Button::Middle => 274,
        Button::Back => 275,
        Button::Forward => 276,
        Button::Other(code) => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::InputEvent;

    #[test]
    fn two_axis_scroll_is_split_for_platform_injection() {
        assert_eq!(
            encode_native_input(InputEvent::Scroll {
                horizontal: 2,
                vertical: -3,
            })
            .len(),
            2
        );
    }

    #[test]
    fn native_key_repeat_is_preserved() {
        let repeat = InputEvent::Key {
            key: Key(30),
            state: ButtonState::Repeated,
        };
        assert_eq!(decode_native_input(1, 30, 2).unwrap(), repeat);
        assert_eq!(encode_native_input(repeat), vec![(1, 30, 2)]);
    }
}
