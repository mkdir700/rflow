use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::core::{Button, ButtonState, InputEvent, Key, Motion as DomainMotion};

pub const PROTOCOL_VERSION: u16 = 3;
pub const MAX_RELIABLE_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MotionDto {
    pub sequence: u64,
    pub timestamp_micros: u64,
    pub dx: i32,
    pub dy: i32,
}

/// Compatibility alias for protocol-v2 callers during the architecture migration.
pub type Motion = MotionDto;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireScreenDescriptor {
    pub stable_id: String,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReliableEvent {
    Hello {
        version: u16,
        screens: Vec<WireScreenDescriptor>,
    },
    ClientHello {
        version: u16,
        screens: Vec<WireScreenDescriptor>,
    },
    ScreenInventory {
        screens: Vec<WireScreenDescriptor>,
    },
    EnterScreen {
        x: i32,
        y: i32,
    },
    Input {
        sequence: u64,
        event_type: u16,
        code: u16,
        value: i32,
    },
    Heartbeat {
        sequence: u64,
    },
    ReleaseAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputConversionError {
    NotInput,
    UnsupportedEventType(u16),
    UnsupportedRelativeAxis(u16),
    CombinedScroll,
}

impl std::fmt::Display for InputConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInput => f.write_str("wire event is not an input event"),
            Self::UnsupportedEventType(value) => write!(f, "unsupported event type {value}"),
            Self::UnsupportedRelativeAxis(value) => {
                write!(f, "unsupported relative axis {value}")
            }
            Self::CombinedScroll => {
                f.write_str("protocol v2 requires horizontal and vertical scroll to be split")
            }
        }
    }
}

impl std::error::Error for InputConversionError {}

pub fn encode_input(
    sequence: u64,
    event: InputEvent,
) -> Result<ReliableEvent, InputConversionError> {
    let (event_type, code, value) = match event {
        InputEvent::Key { key, state } => (1, key.0, encode_state(state)),
        InputEvent::Button { button, state } => (1, encode_button(button), encode_state(state)),
        InputEvent::Scroll {
            horizontal: 0,
            vertical,
        } => (2, 8, vertical),
        InputEvent::Scroll {
            horizontal,
            vertical: 0,
        } => (2, 6, horizontal),
        InputEvent::Scroll { .. } => return Err(InputConversionError::CombinedScroll),
    };
    Ok(ReliableEvent::Input {
        sequence,
        event_type,
        code,
        value,
    })
}

pub fn decode_input(event: &ReliableEvent) -> Result<InputEvent, InputConversionError> {
    let ReliableEvent::Input {
        event_type,
        code,
        value,
        ..
    } = *event
    else {
        return Err(InputConversionError::NotInput);
    };
    match event_type {
        1 if (272..=287).contains(&code) => Ok(InputEvent::Button {
            button: decode_button(code),
            state: decode_state(value),
        }),
        1 => Ok(InputEvent::Key {
            key: Key(code),
            state: decode_state(value),
        }),
        2 if code == 6 => Ok(InputEvent::Scroll {
            horizontal: value,
            vertical: 0,
        }),
        2 if code == 8 => Ok(InputEvent::Scroll {
            horizontal: 0,
            vertical: value,
        }),
        2 => Err(InputConversionError::UnsupportedRelativeAxis(code)),
        value => Err(InputConversionError::UnsupportedEventType(value)),
    }
}

impl From<DomainMotion> for MotionDto {
    fn from(value: DomainMotion) -> Self {
        Self {
            sequence: value.sequence,
            timestamp_micros: value.timestamp_micros,
            dx: value.dx,
            dy: value.dy,
        }
    }
}

impl From<MotionDto> for DomainMotion {
    fn from(value: MotionDto) -> Self {
        Self {
            sequence: value.sequence,
            timestamp_micros: value.timestamp_micros,
            dx: value.dx,
            dy: value.dy,
        }
    }
}

fn encode_state(state: ButtonState) -> i32 {
    match state {
        ButtonState::Pressed => 1,
        ButtonState::Released => 0,
        ButtonState::Repeated => 2,
    }
}

fn decode_state(value: i32) -> ButtonState {
    match value {
        0 => ButtonState::Released,
        2 => ButtonState::Repeated,
        _ => ButtonState::Pressed,
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

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    postcard::to_allocvec(value).context("encode protocol message")
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    postcard::from_bytes(bytes).context("decode protocol message")
}

pub fn encode_frame(event: &ReliableEvent) -> Result<Vec<u8>> {
    let body = encode(event)?;
    if body.len() > MAX_RELIABLE_FRAME {
        bail!("reliable frame exceeds {MAX_RELIABLE_FRAME} bytes");
    }
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_round_trip() {
        let event = MotionDto {
            sequence: 42,
            timestamp_micros: 123_456,
            dx: -7,
            dy: 9,
        };
        let bytes = encode(&event).unwrap();
        assert_eq!(decode::<MotionDto>(&bytes).unwrap(), event);
        assert!(bytes.len() < 32);
    }

    #[test]
    fn domain_input_has_explicit_wire_conversion() {
        let inputs = [
            InputEvent::Key {
                key: Key(30),
                state: ButtonState::Pressed,
            },
            InputEvent::Button {
                button: Button::Left,
                state: ButtonState::Released,
            },
            InputEvent::Scroll {
                horizontal: 0,
                vertical: -1,
            },
            InputEvent::Key {
                key: Key(30),
                state: ButtonState::Repeated,
            },
        ];
        for (sequence, input) in inputs.into_iter().enumerate() {
            let wire = encode_input(sequence as u64, input).unwrap();
            assert_eq!(decode_input(&wire).unwrap(), input);
        }
    }

    #[test]
    fn reliable_frame_has_big_endian_length() {
        let bytes = encode_frame(&ReliableEvent::ReleaseAll).unwrap();
        let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(length, bytes.len() - 4);
    }

    #[test]
    fn reliable_handshake_round_trips_multiple_screens() {
        let event = ReliableEvent::ClientHello {
            version: PROTOCOL_VERSION,
            screens: vec![
                WireScreenDescriptor {
                    stable_id: "display-a".into(),
                    name: "DP-1".into(),
                    width: 2560,
                    height: 1440,
                    primary: true,
                },
                WireScreenDescriptor {
                    stable_id: "display-b".into(),
                    name: "HDMI-1".into(),
                    width: 1920,
                    height: 1080,
                    primary: false,
                },
            ],
        };
        let frame = encode_frame(&event).unwrap();
        assert_eq!(decode::<ReliableEvent>(&frame[4..]).unwrap(), event);
    }
}
