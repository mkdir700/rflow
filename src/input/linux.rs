use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    process::Command,
    sync::mpsc as std_mpsc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
};

use crate::{core::ScreenSize, platform::DetectedScreen};
use anyhow::{Context, Result, bail};
use evdev::{
    AttributeSet, AttributeSetRef, Device, EventType, InputEvent, KeyCode, RelativeAxisCode,
    uinput::VirtualDevice,
};
use serde::Deserialize;

pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;

#[derive(Debug, Clone, Copy)]
pub(crate) enum NativeCapturedEvent {
    Input {
        source: usize,
        sequence: u64,
        event_type: u16,
        code: u16,
        value: i32,
    },
    Motion {
        source: usize,
        sequence: u64,
        timestamp_micros: u64,
        dx: i32,
        dy: i32,
    },
}

pub(crate) type CaptureCallback = Arc<dyn Fn(NativeCapturedEvent) + Send + Sync>;

pub fn validate_capture(paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        anyhow::bail!("Linux host requires at least one --device path");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeviceCapabilities {
    keyboard: bool,
    pointer: bool,
}

pub fn resolve_capture_devices(overrides: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !overrides.is_empty() {
        validate_capture(overrides)?;
        return Ok(overrides.to_vec());
    }

    let entries = fs::read_dir("/dev/input")
        .context("enumerate /dev/input; grant this user read access to evdev devices")?;
    let mut selected = Vec::new();
    let mut denied = Vec::new();
    let mut has_keyboard = false;
    let mut has_pointer = false;
    for entry in entries {
        let entry = entry.context("read /dev/input entry")?;
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        let device = match Device::open(&path) {
            Ok(device) => device,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                denied.push(path);
                continue;
            }
            Err(error) => {
                tracing::debug!(device = %path.display(), %error, "skip unreadable input device");
                continue;
            }
        };
        let name = device.name().unwrap_or_default();
        if is_virtual_input_name(name) {
            continue;
        }
        let capabilities = classify_device(&device);
        if capabilities.keyboard || capabilities.pointer {
            has_keyboard |= capabilities.keyboard;
            has_pointer |= capabilities.pointer;
            selected.push(path);
        }
    }
    selected.sort();
    if has_keyboard && has_pointer {
        for path in &selected {
            tracing::info!(device = %path.display(), "automatically selected input device");
        }
        return Ok(selected);
    }

    let missing = match (has_keyboard, has_pointer) {
        (false, false) => "keyboard and relative pointer",
        (false, true) => "keyboard",
        (true, false) => "relative pointer",
        (true, true) => unreachable!(),
    };
    if denied.is_empty() {
        bail!(
            "automatic evdev discovery found no usable {missing}; pass --device PATH as an advanced override"
        );
    }
    let paths = denied
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "automatic evdev discovery could not read {paths}; grant the current user read access (commonly with an input-group or udev rule), then log out/in, or pass --device PATH"
    )
}

fn classify_device(device: &Device) -> DeviceCapabilities {
    let keys = device.supported_keys();
    let axes = device.supported_relative_axes();
    DeviceCapabilities {
        keyboard: keys.is_some_and(|keys| {
            keys.contains(KeyCode::KEY_A)
                && keys.contains(KeyCode::KEY_Z)
                && keys.contains(KeyCode::KEY_ENTER)
                && keys.contains(KeyCode::KEY_SPACE)
        }),
        pointer: keys.is_some_and(|keys| keys.contains(KeyCode::BTN_LEFT))
            && axes.is_some_and(|axes| {
                axes.contains(RelativeAxisCode::REL_X) && axes.contains(RelativeAxisCode::REL_Y)
            }),
    }
}

fn is_virtual_input_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "rflow", "virtual", "uinput", "deskflow", "synergy", "barrier",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

pub fn screens() -> Result<Vec<DetectedScreen>> {
    hyprland_screens().or_else(|hyprland_error| {
        xrandr_screens().with_context(|| {
            format!(
                "automatic screen discovery failed (Hyprland: {hyprland_error:#}); pass an explicit per-screen override"
            )
        })
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HyprlandMonitor {
    name: String,
    description: String,
    #[serde(default)]
    make: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    serial: String,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    scale: f64,
    focused: bool,
}

fn hyprland_screens() -> Result<Vec<DetectedScreen>> {
    let output = Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .context("run `hyprctl -j monitors`")?;
    if !output.status.success() {
        bail!("`hyprctl -j monitors` exited with {}", output.status);
    }
    let monitors: Vec<HyprlandMonitor> =
        serde_json::from_slice(&output.stdout).context("decode Hyprland monitor list")?;
    let screens = monitors
        .into_iter()
        .map(|monitor| {
            if !monitor.scale.is_finite() || monitor.scale <= 0.0 {
                bail!("Hyprland reported invalid scale for {}", monitor.name);
            }
            let width = (f64::from(monitor.width) / monitor.scale).round() as i32;
            let height = (f64::from(monitor.height) / monitor.scale).round() as i32;
            Ok(DetectedScreen {
                stable_id: hyprland_stable_id(&monitor),
                name: if monitor.description.trim().is_empty() {
                    monitor.name
                } else {
                    monitor.description
                },
                logical_size: ScreenSize::new(width, height).map_err(anyhow::Error::msg)?,
                scale: monitor.scale,
                x: monitor.x,
                y: monitor.y,
                primary: monitor.focused,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if screens.is_empty() {
        bail!("Hyprland reported no active monitors");
    }
    Ok(screens)
}

fn hyprland_stable_id(monitor: &HyprlandMonitor) -> String {
    if monitor.serial.trim().is_empty() {
        format!("hyprland:connector:{}", monitor.name)
    } else {
        format!(
            "hyprland:display:{}:{}:{}",
            monitor.make.trim(),
            monitor.model.trim(),
            monitor.serial.trim()
        )
    }
}

fn xrandr_screens() -> Result<Vec<DetectedScreen>> {
    let output = Command::new("xrandr")
        .arg("--listactivemonitors")
        .output()
        .context("run `xrandr --listactivemonitors`")?;
    if !output.status.success() {
        bail!(
            "`xrandr --listactivemonitors` exited with {}",
            output.status
        );
    }
    parse_xrandr_monitors(&String::from_utf8(output.stdout).context("decode xrandr output")?)
}

fn parse_xrandr_monitors(output: &str) -> Result<Vec<DetectedScreen>> {
    let mut screens = Vec::new();
    for line in output.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let flags_and_name = fields[1];
        let primary = flags_and_name.contains('*');
        let connector = fields.last().expect("xrandr line has fields").to_string();
        let geometry = fields[2];
        let (width, rest) = geometry
            .split_once('/')
            .context("xrandr monitor width is missing")?;
        let (_, rest) = rest
            .split_once('x')
            .context("xrandr monitor height is missing")?;
        let (height, positions) = rest
            .split_once('/')
            .context("xrandr monitor physical height is missing")?;
        let positions = positions
            .find(['+', '-'])
            .map(|index| &positions[index..])
            .context("xrandr monitor position is missing")?;
        let (x, y) = parse_signed_coordinates(positions)?;
        screens.push(DetectedScreen {
            stable_id: format!("xrandr:{connector}"),
            name: connector,
            logical_size: ScreenSize::new(width.parse()?, height.parse()?)
                .map_err(anyhow::Error::msg)?,
            scale: 1.0,
            x,
            y,
            primary,
        });
    }
    if screens.is_empty() {
        bail!("xrandr reported no active monitors");
    }
    if !screens.iter().any(|screen| screen.primary) {
        screens[0].primary = true;
    }
    Ok(screens)
}

fn parse_signed_coordinates(value: &str) -> Result<(i32, i32)> {
    let split = value[1..]
        .find(['+', '-'])
        .map(|index| index + 1)
        .context("monitor position needs two coordinates")?;
    Ok((value[..split].parse()?, value[split..].parse()?))
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
    callback: CaptureCallback,
    stop: Arc<AtomicBool>,
) -> Result<Vec<std::thread::JoinHandle<()>>> {
    let sequence = Arc::new(AtomicU64::new(0));
    let expected = paths.len();
    let (ready_tx, ready_rx) = std_mpsc::channel();
    let handles: Vec<std::thread::JoinHandle<()>> = paths
        .into_iter()
        .enumerate()
        .map(|(source, path)| {
            let callback = callback.clone();
            let sequence = sequence.clone();
            let ready = ready_tx.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                if let Err(error) =
                    capture_device(source, &path, grab, callback, sequence, &ready, &stop)
                {
                    let _ = ready.send(Err(format!("{error:#}")));
                    tracing::error!(device = %path.display(), %error, "input capture stopped");
                }
            })
        })
        .collect();
    drop(ready_tx);
    let mut startup_error = None;
    for _ in 0..expected {
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                startup_error = Some(format!("input capture failed before startup: {error}"));
                break;
            }
            Err(_) => {
                startup_error = Some("input capture stopped before startup completed".to_owned());
                break;
            }
        }
    }
    if let Some(error) = startup_error {
        stop.store(true, Ordering::Release);
        for handle in handles {
            let _ = handle.join();
        }
        anyhow::bail!(error);
    }
    Ok(handles)
}

fn capture_device(
    source: usize,
    path: &PathBuf,
    grab: bool,
    callback: CaptureCallback,
    sequence: Arc<AtomicU64>,
    ready: &std_mpsc::Sender<std::result::Result<(), String>>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut device =
        Device::open(path).with_context(|| format!("open input device {}", path.display()))?;
    if grab {
        device
            .grab()
            .with_context(|| format!("grab {}", path.display()))?;
    }
    device
        .set_nonblocking(true)
        .with_context(|| format!("set {} nonblocking", path.display()))?;
    tracing::info!(device = %path.display(), name = ?device.name(), grab, "capturing input");
    let _ = ready.send(Ok(()));

    while !stop.load(Ordering::Acquire) {
        let events: Vec<_> = match device.fetch_events() {
            Ok(events) => events.collect(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
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
                callback(NativeCapturedEvent::Input {
                    source,
                    sequence,
                    event_type,
                    code,
                    value: event.value(),
                });
            }
        }
        if dx != 0 || dy != 0 {
            let sequence = sequence.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            let timestamp_micros = SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap_or_default()
                .as_micros() as u64;
            callback(NativeCapturedEvent::Motion {
                source,
                sequence,
                timestamp_micros,
                dx,
                dy,
            });
        }
    }
    Ok(())
}

pub struct Injector {
    devices: Vec<VirtualDevice>,
    pointer_source: usize,
}

impl Injector {
    pub fn new(paths: &[PathBuf]) -> Result<Self> {
        if paths.is_empty() {
            return Ok(Self {
                devices: vec![generic_remote_device()?],
                pointer_source: 0,
            });
        }
        let hypr_before = hypr_keyboards();
        let mut devices = Vec::with_capacity(paths.len());
        let mut keyboard_profiles = Vec::new();
        let mut pointer_source = None;
        for (source_index, path) in paths.iter().enumerate() {
            let source = Device::open(path)
                .with_context(|| format!("inspect input device {}", path.display()))?;
            let name = source.name().unwrap_or("rflow input");
            if let Some(profile) = hypr_before
                .as_ref()
                .and_then(|keyboards| keyboards.iter().find(|keyboard| keyboard.name == name))
            {
                keyboard_profiles.push(profile.clone());
            }
            let mut builder = VirtualDevice::builder()
                .context("open /dev/uinput")?
                .name(name);
            if let Some(keys) = source.supported_keys() {
                builder = builder.with_keys(keys)?;
            }
            if let Some(source_axes) = source.supported_relative_axes() {
                let axes = forwarded_relative_axes(source_axes);
                if axes.contains(RelativeAxisCode::REL_X)
                    && axes.contains(RelativeAxisCode::REL_Y)
                    && pointer_source.is_none()
                {
                    pointer_source = Some(source_index);
                }
                builder = builder.with_relative_axes(&axes)?;
            }
            devices.push(
                builder
                    .build()
                    .with_context(|| format!("create virtual clone of {name}"))?,
            );
        }
        if let Some(before) = hypr_before {
            sync_hypr_keyboard_profiles(&before, &keyboard_profiles);
        }
        Ok(Self {
            devices,
            pointer_source: pointer_source.unwrap_or(0),
        })
    }

    pub fn emit_motion(&mut self, source: usize, dx: i32, dy: i32) -> Result<()> {
        let mut events = Vec::with_capacity(2);
        if dx != 0 {
            events.push(InputEvent::new(EV_REL, REL_X, dx));
        }
        if dy != 0 {
            events.push(InputEvent::new(EV_REL, REL_Y, dy));
        }
        if !events.is_empty() {
            self.device(source)?
                .emit(&events)
                .context("inject pointer motion")?;
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
        self.emit_motion(self.pointer_source, -1_000_000, -1_000_000)?;
        self.emit_motion(self.pointer_source, x.max(0), y.max(0))
    }

    pub fn emit_raw(
        &mut self,
        source: usize,
        event_type: u16,
        code: u16,
        value: i32,
    ) -> Result<()> {
        if event_type != EventType::KEY.0 && event_type != EventType::RELATIVE.0 {
            return Ok(());
        }
        self.device(source)?
            .emit(&[InputEvent::new(event_type, code, value)])
            .context("inject input event")
    }

    fn device(&mut self, source: usize) -> Result<&mut VirtualDevice> {
        self.devices
            .get_mut(source)
            .with_context(|| format!("input source {source} has no virtual device"))
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct HyprKeyboard {
    address: String,
    name: String,
    rules: String,
    model: String,
    layout: String,
    variant: String,
    options: String,
}

#[derive(Deserialize)]
struct HyprDevices {
    keyboards: Vec<HyprKeyboard>,
}

fn hypr_keyboards() -> Option<Vec<HyprKeyboard>> {
    let output = Command::new("hyprctl")
        .args(["devices", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<HyprDevices>(&output.stdout)
        .ok()
        .map(|devices| devices.keyboards)
}

fn sync_hypr_keyboard_profiles(before: &[HyprKeyboard], profiles: &[HyprKeyboard]) {
    if profiles.is_empty() {
        return;
    }
    let old_addresses: HashSet<&str> = before
        .iter()
        .map(|keyboard| keyboard.address.as_str())
        .collect();
    for _ in 0..20 {
        if let Some(current) = hypr_keyboards() {
            let mut pending = false;
            for profile in profiles {
                let virtual_keyboard = current.iter().find(|keyboard| {
                    !old_addresses.contains(keyboard.address.as_str())
                        && virtual_device_name_matches(&keyboard.name, &profile.name)
                });
                if let Some(virtual_keyboard) = virtual_keyboard {
                    apply_hypr_keyboard_profile(&virtual_keyboard.name, profile);
                } else {
                    pending = true;
                }
            }
            if !pending {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    tracing::warn!("timed out synchronizing virtual keyboard settings with Hyprland");
}

fn virtual_device_name_matches(candidate: &str, physical: &str) -> bool {
    candidate == physical
        || candidate
            .strip_prefix(physical)
            .and_then(|suffix| suffix.strip_prefix('-'))
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            })
}

fn apply_hypr_keyboard_profile(name: &str, profile: &HyprKeyboard) {
    let string = |value: &str| serde_json::to_string(value).expect("serialize Hyprland string");
    let lua = format!(
        "hl.device({{ name = {}, kb_rules = {}, kb_model = {}, kb_layout = {}, kb_variant = {}, kb_options = {} }})",
        string(name),
        string(&profile.rules),
        string(&profile.model),
        string(&profile.layout),
        string(&profile.variant),
        string(&profile.options),
    );
    match Command::new("hyprctl").args(["eval", &lua]).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            keyboard = name,
            error = %String::from_utf8_lossy(&output.stderr),
            "failed to apply physical keyboard settings to virtual keyboard"
        ),
        Err(error) => tracing::warn!(
            keyboard = name,
            %error,
            "failed to invoke Hyprland while configuring virtual keyboard"
        ),
    }
}

fn forwarded_relative_axes(
    source: &AttributeSetRef<RelativeAxisCode>,
) -> AttributeSet<RelativeAxisCode> {
    source
        .into_iter()
        .filter(|axis| {
            !matches!(
                *axis,
                RelativeAxisCode::REL_WHEEL_HI_RES | RelativeAxisCode::REL_HWHEEL_HI_RES
            )
        })
        .collect()
}

fn generic_remote_device() -> Result<VirtualDevice> {
    let mut keys = evdev::AttributeSet::<KeyCode>::new();
    for code in 0..=0x2ff {
        keys.insert(KeyCode(code));
    }
    let mut axes = evdev::AttributeSet::<RelativeAxisCode>::new();
    for axis in [
        RelativeAxisCode::REL_X,
        RelativeAxisCode::REL_Y,
        RelativeAxisCode::REL_WHEEL,
        RelativeAxisCode::REL_HWHEEL,
    ] {
        axes.insert(axis);
    }
    VirtualDevice::builder()
        .context("open /dev/uinput")?
        .name("rflow remote input")
        .with_keys(&keys)?
        .with_relative_axes(&axes)?
        .build()
        .context("create rflow remote input device")
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn virtual_input_devices_are_excluded_without_rejecting_physical_names() {
        assert!(is_virtual_input_name("rflow virtual keyboard"));
        assert!(is_virtual_input_name("Deskflow pointer"));
        assert!(!is_virtual_input_name("Logitech USB Receiver"));
    }

    #[test]
    fn forwarded_axes_do_not_claim_high_resolution_scroll() {
        let source: AttributeSet<RelativeAxisCode> = [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ]
        .into_iter()
        .collect();

        let forwarded = forwarded_relative_axes(&source);

        assert!(forwarded.contains(RelativeAxisCode::REL_WHEEL));
        assert!(forwarded.contains(RelativeAxisCode::REL_HWHEEL));
        assert!(!forwarded.contains(RelativeAxisCode::REL_WHEEL_HI_RES));
        assert!(!forwarded.contains(RelativeAxisCode::REL_HWHEEL_HI_RES));
    }

    #[test]
    fn hyprland_suffixes_are_matched_without_accepting_similar_device_names() {
        assert!(virtual_device_name_matches(
            "hhkb-hybrid_3-keyboard-1",
            "hhkb-hybrid_3-keyboard"
        ));
        assert!(virtual_device_name_matches(
            "hhkb-hybrid_3-keyboard-12",
            "hhkb-hybrid_3-keyboard"
        ));
        assert!(!virtual_device_name_matches(
            "hhkb-hybrid_3-keyboard-extra",
            "hhkb-hybrid_3-keyboard"
        ));
        assert!(!virtual_device_name_matches(
            "hhkb-hybrid_3-keyboard-pro-1",
            "hhkb-hybrid_3-keyboard"
        ));
    }

    #[test]
    fn xrandr_monitors_preserve_each_logical_screen_and_signed_position() {
        let screens = parse_xrandr_monitors(
            "Monitors: 2\n 0: +*DP-1 1920/509x1080/286+0+0  DP-1\n 1: +HDMI-1 2560/600x1440/340-2560+120  HDMI-1\n",
        )
        .unwrap();
        assert_eq!(screens.len(), 2);
        assert_eq!(
            screens[0].logical_size,
            ScreenSize::new(1920, 1080).unwrap()
        );
        assert!(screens[0].primary);
        assert_eq!((screens[1].x, screens[1].y), (-2560, 120));
        assert_eq!(screens[1].stable_id, "xrandr:HDMI-1");
    }
}
