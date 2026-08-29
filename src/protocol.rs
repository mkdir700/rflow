use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_RELIABLE_FRAME: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Motion {
    pub sequence: u64,
    pub timestamp_micros: u64,
    pub dx: i32,
    pub dy: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReliableEvent {
    Hello {
        version: u16,
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
        let event = Motion {
            sequence: 42,
            timestamp_micros: 123_456,
            dx: -7,
            dy: 9,
        };
        let bytes = encode(&event).unwrap();
        assert_eq!(decode::<Motion>(&bytes).unwrap(), event);
        assert!(bytes.len() < 32);
    }

    #[test]
    fn reliable_frame_has_big_endian_length() {
        let bytes = encode_frame(&ReliableEvent::ReleaseAll).unwrap();
        let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(length, bytes.len() - 4);
    }
}
