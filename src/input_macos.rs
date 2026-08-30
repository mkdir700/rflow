use std::{ffi::c_void, path::PathBuf};

use anyhow::{Result, bail};
use tokio::sync::{mpsc, watch};

use crate::protocol::{Motion, ReliableEvent};

pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state_id: i32) -> CGEventSourceRef;
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        ...
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CFRelease(value: *const c_void);
}

pub fn spawn_capture(
    _paths: Vec<PathBuf>,
    _grab: bool,
    _reliable: mpsc::Sender<ReliableEvent>,
    _motion: watch::Sender<Option<Motion>>,
) -> Vec<std::thread::JoinHandle<()>> {
    tracing::error!("macOS input capture is not implemented; use macOS as `rflow host`");
    Vec::new()
}

pub struct Injector {
    source: CGEventSourceRef,
}

impl Injector {
    pub fn new() -> Result<Self> {
        let source = unsafe { CGEventSourceCreate(1) };
        if source.is_null() {
            bail!("create macOS HID event source");
        }
        Ok(Self { source })
    }

    pub fn emit_motion(&mut self, dx: i32, dy: i32) -> Result<()> {
        let position = self.pointer_position()?;
        self.post_mouse(
            5,
            CGPoint {
                x: position.x + dx as f64,
                y: position.y + dy as f64,
            },
            0,
        )
    }

    pub fn emit_raw(&mut self, event_type: u16, code: u16, value: i32) -> Result<()> {
        match event_type {
            EV_KEY => self.emit_key(code, value),
            EV_REL if code == REL_WHEEL => self.emit_scroll(value, 0),
            EV_REL if code == REL_HWHEEL => self.emit_scroll(0, value),
            _ => Ok(()),
        }
    }

    fn emit_key(&mut self, code: u16, value: i32) -> Result<()> {
        if let Some((down, up, button)) = mouse_button(code) {
            return self.post_mouse(
                if value == 0 { up } else { down },
                self.pointer_position()?,
                button,
            );
        }
        let Some(keycode) = linux_key_to_macos(code) else {
            tracing::debug!(code, "ignoring unmapped Linux key code on macOS");
            return Ok(());
        };
        let event = unsafe { CGEventCreateKeyboardEvent(self.source, keycode, value != 0) };
        post_and_release(event, "create macOS keyboard event")
    }

    fn emit_scroll(&mut self, vertical: i32, horizontal: i32) -> Result<()> {
        let event =
            unsafe { CGEventCreateScrollWheelEvent(self.source, 1, 2, vertical, horizontal) };
        post_and_release(event, "create macOS scroll event")
    }

    fn pointer_position(&self) -> Result<CGPoint> {
        let event = unsafe { CGEventCreate(std::ptr::null_mut()) };
        if event.is_null() {
            bail!("read macOS pointer position");
        }
        let position = unsafe { CGEventGetLocation(event) };
        unsafe { CFRelease(event) };
        Ok(position)
    }

    fn post_mouse(&self, event_type: u32, position: CGPoint, button: u32) -> Result<()> {
        let event = unsafe { CGEventCreateMouseEvent(self.source, event_type, position, button) };
        post_and_release(event, "create macOS mouse event")
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        unsafe { CFRelease(self.source) };
    }
}

fn post_and_release(event: CGEventRef, message: &str) -> Result<()> {
    if event.is_null() {
        bail!("{message}");
    }
    unsafe {
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
}

fn mouse_button(code: u16) -> Option<(u32, u32, u32)> {
    match code {
        272 => Some((1, 2, 0)),
        273 => Some((3, 4, 1)),
        274 => Some((25, 26, 2)),
        275 => Some((25, 26, 3)),
        276 => Some((25, 26, 4)),
        _ => None,
    }
}

fn linux_key_to_macos(code: u16) -> Option<u16> {
    Some(match code {
        1 => 53,
        2 => 18,
        3 => 19,
        4 => 20,
        5 => 21,
        6 => 23,
        7 => 22,
        8 => 26,
        9 => 28,
        10 => 25,
        11 => 29,
        12 => 27,
        13 => 24,
        14 => 51,
        15 => 48,
        16..=25 => [12, 13, 14, 15, 17, 16, 32, 34, 31, 35][(code - 16) as usize],
        26 => 33,
        27 => 30,
        28 | 96 => 36,
        29 => 59,
        97 => 62,
        30..=38 => [0, 1, 2, 3, 5, 4, 38, 40, 37][(code - 30) as usize],
        39 => 41,
        40 => 39,
        41 => 50,
        42 => 56,
        54 => 60,
        43 => 42,
        44..=50 => [6, 7, 8, 9, 11, 45, 46][(code - 44) as usize],
        51 => 43,
        52 => 47,
        53 => 44,
        56 => 58,
        100 => 61,
        57 => 49,
        58 => 57,
        59 => 122,
        60 => 120,
        61 => 99,
        62 => 118,
        63 => 96,
        64 => 97,
        65 => 98,
        66 => 100,
        67 => 101,
        68 => 109,
        87 => 103,
        88 => 111,
        102 => 115,
        103 => 126,
        104 => 116,
        105 => 123,
        106 => 124,
        107 => 119,
        108 => 125,
        109 => 121,
        110 => 114,
        111 => 117,
        125 | 126 => 55,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_linux_keys() {
        assert_eq!(linux_key_to_macos(30), Some(0));
        assert_eq!(linux_key_to_macos(28), Some(36));
        assert_eq!(linux_key_to_macos(103), Some(126));
        assert_eq!(linux_key_to_macos(700), None);
    }
}
