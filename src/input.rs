use std::{
    path::PathBuf,
    process::Command,
    sync::mpsc as std_mpsc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use anyhow::{Context, Result};
use evdev::{
    AttributeSet, Device, EventType, InputEvent, KeyCode, RelativeAxisCode, uinput::VirtualDevice,
};
use tokio::sync::{mpsc, watch};

use crate::protocol::{Motion, ReliableEvent};
use crate::router::ScreenSize;

pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;

pub fn validate_capture(paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        anyhow::bail!("Linux connect requires at least one --device path");
    }
    Ok(())
}

pub fn screen_size() -> Result<ScreenSize> {
    anyhow::bail!("automatic screen size is unavailable on Linux; pass --size WIDTHxHEIGHT")
}

pub fn cursor_position() -> Option<(i32, i32)> {
    let output = Command::new("hyprctl").arg("cursorpos").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let (x, y) = text.trim().split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

pub fn spawn_capture(
    paths: Vec<PathBuf>,
    grab: bool,
    reliable: mpsc::Sender<ReliableEvent>,
    motion: watch::Sender<Option<Motion>>,
) -> Result<Vec<std::thread::JoinHandle<()>>> {
    let sequence = Arc::new(AtomicU64::new(0));
    let expected = paths.len();
    let (ready_tx, ready_rx) = std_mpsc::channel();
    let handles = paths
        .into_iter()
        .map(|path| {
            let reliable = reliable.clone();
            let motion = motion.clone();
            let sequence = sequence.clone();
            let ready = ready_tx.clone();
            std::thread::spawn(move || {
                if let Err(error) = capture_device(&path, grab, reliable, motion, sequence, &ready)
                {
                    let _ = ready.send(Err(format!("{error:#}")));
                    tracing::error!(device = %path.display(), %error, "input capture stopped");
                }
            })
        })
        .collect();
    drop(ready_tx);
    for _ in 0..expected {
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => anyhow::bail!("input capture failed before startup: {error}"),
            Err(_) => anyhow::bail!("input capture stopped before startup completed"),
        }
    }
    Ok(handles)
}

fn capture_device(
    path: &PathBuf,
    grab: bool,
    reliable: mpsc::Sender<ReliableEvent>,
    motion: watch::Sender<Option<Motion>>,
    sequence: Arc<AtomicU64>,
    ready: &std_mpsc::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let mut device =
        Device::open(path).with_context(|| format!("open input device {}", path.display()))?;
    if grab {
        device
            .grab()
            .with_context(|| format!("grab {}", path.display()))?;
    }
    tracing::info!(device = %path.display(), name = ?device.name(), grab, "capturing input");
    let _ = ready.send(Ok(()));

    loop {
        let events: Vec<_> = device.fetch_events()?.collect();
        let mut dx = 0_i32;
        let mut dy = 0_i32;
        for event in events {
            let event_type = event.event_type().0;
            let code = event.code();
            if event_type == EV_REL && code == REL_X {
                dx = dx.saturating_add(event.value());
            } else if event_type == EV_REL && code == REL_Y {
                dy = dy.saturating_add(event.value());
            } else if event_type == EV_KEY || event_type == EV_REL {
                let sequence = sequence.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                reliable.blocking_send(ReliableEvent::Input {
                    sequence,
                    event_type,
                    code,
                    value: event.value(),
                })?;
            }
        }
        if dx != 0 || dy != 0 {
            let sequence = sequence.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            let timestamp_micros = SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap_or_default()
                .as_micros() as u64;
            motion.send_replace(Some(Motion {
                sequence,
                timestamp_micros,
                dx,
                dy,
            }));
        }
    }
}

pub struct Injector {
    device: VirtualDevice,
}

impl Injector {
    pub fn new() -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 0..=0x2ff {
            keys.insert(KeyCode(code));
        }
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        for code in 0..=0x0c {
            axes.insert(RelativeAxisCode(code));
        }
        let device = VirtualDevice::builder()
            .context("open /dev/uinput")?
            .name("rflow virtual input")
            .with_keys(&keys)?
            .with_relative_axes(&axes)?
            .build()
            .context("create rflow virtual input device")?;
        Ok(Self { device })
    }

    pub fn emit_motion(&mut self, dx: i32, dy: i32) -> Result<()> {
        let mut events = Vec::with_capacity(2);
        if dx != 0 {
            events.push(InputEvent::new(EV_REL, REL_X, dx));
        }
        if dy != 0 {
            events.push(InputEvent::new(EV_REL, REL_Y, dy));
        }
        if !events.is_empty() {
            self.device.emit(&events).context("inject pointer motion")?;
        }
        Ok(())
    }

    pub fn set_cursor_position(&mut self, x: i32, y: i32) -> Result<()> {
        let lua = format!("hl.dispatch(hl.dsp.cursor.move({{ x = {x}, y = {y} }}))");
        if Command::new("hyprctl")
            .args(["eval", &lua])
            .status()
            .is_ok_and(|status| status.success())
        {
            return Ok(());
        }
        self.emit_motion(-1_000_000, -1_000_000)?;
        self.emit_motion(x.max(0), y.max(0))
    }

    pub fn emit_raw(&mut self, event_type: u16, code: u16, value: i32) -> Result<()> {
        if event_type != EventType::KEY.0 && event_type != EventType::RELATIVE.0 {
            return Ok(());
        }
        self.device
            .emit(&[InputEvent::new(event_type, code, value)])
            .context("inject input event")
    }
}
