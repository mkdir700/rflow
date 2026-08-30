use std::path::PathBuf;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
};

use crate::protocol::{Motion, ReliableEvent};

pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

pub fn spawn_capture(
    _paths: Vec<PathBuf>,
    _grab: bool,
    _reliable: mpsc::Sender<ReliableEvent>,
    _motion: watch::Sender<Option<Motion>>,
) -> Vec<std::thread::JoinHandle<()>> {
    tracing::error!("Windows input capture is not implemented; use Windows as `rflow host`");
    Vec::new()
}

pub struct Injector;

impl Injector {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    pub fn emit_motion(&mut self, dx: i32, dy: i32) -> Result<()> {
        send_mouse(dx, dy, 0, MOUSEEVENTF_MOVE)
    }

    pub fn emit_raw(&mut self, event_type: u16, code: u16, value: i32) -> Result<()> {
        match event_type {
            EV_KEY => self.emit_key(code, value),
            EV_REL if code == REL_WHEEL => {
                send_mouse(0, 0, (value * 120) as u32, MOUSEEVENTF_WHEEL)
            }
            EV_REL if code == REL_HWHEEL => {
                send_mouse(0, 0, (value * 120) as u32, MOUSEEVENTF_HWHEEL)
            }
            _ => Ok(()),
        }
    }

    fn emit_key(&mut self, code: u16, value: i32) -> Result<()> {
        if let Some((down, up, data)) = mouse_button(code) {
            return send_mouse(0, 0, data, if value == 0 { up } else { down });
        }
        let Some(vk) = linux_key_to_vk(code) else {
            tracing::debug!(code, "ignoring unmapped Linux key code on Windows");
            return Ok(());
        };
        let flags = if value == 0 { KEYEVENTF_KEYUP } else { 0 };
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send_inputs(&[input])
    }
}

fn send_mouse(dx: i32, dy: i32, data: u32, flags: u32) -> Result<()> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_inputs(&[input])
}

fn send_inputs(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    if sent != inputs.len() as u32 {
        Err(anyhow::Error::new(std::io::Error::last_os_error()).context("inject Windows input"))
    } else {
        Ok(())
    }
}

fn mouse_button(code: u16) -> Option<(u32, u32, u32)> {
    match code {
        272 => Some((MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0)),
        273 => Some((MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0)),
        274 => Some((MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0)),
        275 => Some((MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1)),
        276 => Some((MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2)),
        _ => None,
    }
}

fn linux_key_to_vk(code: u16) -> Option<u16> {
    Some(match code {
        1 => 0x1b,
        2..=10 => 0x31 + code - 2,
        11 => 0x30,
        12 => 0xbd,
        13 => 0xbb,
        14 => 0x08,
        15 => 0x09,
        16..=25 => b"QWERTYUIOP"[(code - 16) as usize] as u16,
        26 => 0xdb,
        27 => 0xdd,
        28 | 96 => 0x0d,
        29 | 97 => 0x11,
        30..=38 => b"ASDFGHJKL"[(code - 30) as usize] as u16,
        39 => 0xba,
        40 => 0xde,
        41 => 0xc0,
        42 | 54 => 0x10,
        43 => 0xdc,
        44..=50 => b"ZXCVBNM"[(code - 44) as usize] as u16,
        51 => 0xbc,
        52 => 0xbe,
        53 | 98 => 0xbf,
        55 => 0x6a,
        56 | 100 => 0x12,
        57 => 0x20,
        58 => 0x14,
        59..=68 => 0x70 + code - 59,
        69 => 0x90,
        70 => 0x91,
        71 => 0x67,
        72 => 0x68,
        73 => 0x69,
        74 => 0x6d,
        75 => 0x64,
        76 => 0x65,
        77 => 0x66,
        78 => 0x6b,
        79 => 0x61,
        80 => 0x62,
        81 => 0x63,
        82 => 0x60,
        83 => 0x6e,
        87..=88 => 0x7a + code - 87,
        99 => 0x2c,
        102 => 0x24,
        103 => 0x26,
        104 => 0x21,
        105 => 0x25,
        106 => 0x27,
        107 => 0x23,
        108 => 0x28,
        109 => 0x22,
        110 => 0x2d,
        111 => 0x2e,
        125 => 0x5b,
        126 => 0x5c,
        127 => 0x5d,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_linux_keys() {
        assert_eq!(linux_key_to_vk(30), Some(b'A' as u16));
        assert_eq!(linux_key_to_vk(28), Some(0x0d));
        assert_eq!(linux_key_to_vk(103), Some(0x26));
        assert_eq!(linux_key_to_vk(700), None);
    }
}
