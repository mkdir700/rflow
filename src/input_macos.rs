use std::{
    collections::HashSet,
    ffi::c_void,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use anyhow::{Result, bail};
use tokio::sync::{mpsc, watch};

use crate::protocol::{Motion, ReliableEvent};
use crate::router::ScreenSize;

pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CFStringRef = *const c_void;
type CGEventTapCallback =
    Option<unsafe extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef>;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn CGEventCreate(source: *mut c_void) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: *mut c_void,
        mouse_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(
        source: *mut c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent(
        source: *mut c_void,
        units: u32,
        wheel_count: u32,
        ...
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayShowCursor(display: u32) -> i32;
    fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;
    fn CGWarpMouseCursorPosition(position: CGPoint) -> i32;
    fn _CGSDefaultConnection() -> i32;
    fn CGSSetConnectionProperty(
        connection: i32,
        target_connection: i32,
        key: CFStringRef,
        value: *const c_void,
    ) -> i32;
    fn CFRelease(value: *const c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: *const c_void;
    static kCFBooleanTrue: *const c_void;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        text: *const i8,
        encoding: u32,
    ) -> CFStringRef;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: *const c_void);
    fn CFRunLoopRun();
}

const EVENT_LEFT_DOWN: u32 = 1;
const EVENT_LEFT_UP: u32 = 2;
const EVENT_RIGHT_DOWN: u32 = 3;
const EVENT_RIGHT_UP: u32 = 4;
const EVENT_MOUSE_MOVED: u32 = 5;
const EVENT_LEFT_DRAGGED: u32 = 6;
const EVENT_RIGHT_DRAGGED: u32 = 7;
const EVENT_KEY_DOWN: u32 = 10;
const EVENT_KEY_UP: u32 = 11;
const EVENT_FLAGS_CHANGED: u32 = 12;
const EVENT_SCROLL: u32 = 22;
const EVENT_OTHER_DOWN: u32 = 25;
const EVENT_OTHER_UP: u32 = 26;
const EVENT_OTHER_DRAGGED: u32 = 27;
const FLAG_CAPS_LOCK: u64 = 1 << 16;
const FLAG_SHIFT: u64 = 1 << 17;
const FLAG_CONTROL: u64 = 1 << 18;
const FLAG_OPTION: u64 = 1 << 19;
const FLAG_COMMAND: u64 = 1 << 20;

struct CaptureContext {
    reliable: mpsc::Sender<ReliableEvent>,
    motion: watch::Sender<Option<Motion>>,
    sequence: AtomicU64,
    modifiers: Mutex<HashSet<u16>>,
    grab: bool,
}

pub fn validate_capture(_paths: &[PathBuf]) -> Result<()> {
    Ok(())
}

pub fn screen_size() -> Result<ScreenSize> {
    let display = unsafe { CGMainDisplayID() };
    let bounds = unsafe { CGDisplayBounds(display) };
    ScreenSize::new(bounds.size.width as i32, bounds.size.height as i32).map_err(anyhow::Error::msg)
}

pub fn cursor_position() -> Option<(i32, i32)> {
    let event = unsafe { CGEventCreate(std::ptr::null_mut()) };
    if event.is_null() {
        return None;
    }
    let position = unsafe { CGEventGetLocation(event) };
    unsafe { CFRelease(event) };
    Some((position.x as i32, position.y as i32))
}

pub fn spawn_capture(
    _paths: Vec<PathBuf>,
    grab: bool,
    reliable: mpsc::Sender<ReliableEvent>,
    motion: watch::Sender<Option<Motion>>,
) -> Vec<std::thread::JoinHandle<()>> {
    vec![std::thread::spawn(move || {
        if let Err(error) = capture_events(grab, reliable, motion) {
            tracing::error!(%error, "macOS input capture stopped");
        }
    })]
}

fn capture_events(
    grab: bool,
    reliable: mpsc::Sender<ReliableEvent>,
    motion: watch::Sender<Option<Motion>>,
) -> Result<()> {
    let context = Box::new(CaptureContext {
        reliable,
        motion,
        sequence: AtomicU64::new(0),
        modifiers: Mutex::new(HashSet::new()),
        grab,
    });
    let context = Box::into_raw(context);
    let mask = [
        EVENT_LEFT_DOWN,
        EVENT_LEFT_UP,
        EVENT_RIGHT_DOWN,
        EVENT_RIGHT_UP,
        EVENT_MOUSE_MOVED,
        EVENT_LEFT_DRAGGED,
        EVENT_RIGHT_DRAGGED,
        EVENT_KEY_DOWN,
        EVENT_KEY_UP,
        EVENT_FLAGS_CHANGED,
        EVENT_SCROLL,
        EVENT_OTHER_DOWN,
        EVENT_OTHER_UP,
        EVENT_OTHER_DRAGGED,
    ]
    .into_iter()
    .fold(0_u64, |mask, event_type| mask | (1_u64 << event_type));
    let tap = unsafe {
        CGEventTapCreate(
            0,
            0,
            if grab { 0 } else { 1 },
            mask,
            Some(event_callback),
            context.cast(),
        )
    };
    if tap.is_null() {
        unsafe { drop(Box::from_raw(context)) };
        bail!("create macOS event tap; grant Accessibility and Input Monitoring permissions");
    }
    let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0) };
    if source.is_null() {
        unsafe {
            CFRelease(tap);
            drop(Box::from_raw(context));
        }
        bail!("create macOS event-tap run-loop source");
    }
    unsafe {
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
    }
    tracing::info!(grab, "capturing macOS keyboard and mouse input");
    unsafe { CFRunLoopRun() };
    unsafe {
        CFRelease(source);
        CFRelease(tap);
        drop(Box::from_raw(context));
    }
    Ok(())
}

unsafe extern "C" fn event_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let context = unsafe { &*(user_info as *const CaptureContext) };
    handle_captured_event(context, event_type, event);
    if context.grab {
        std::ptr::null_mut()
    } else {
        event
    }
}

fn handle_captured_event(context: &CaptureContext, event_type: u32, event: CGEventRef) {
    match event_type {
        EVENT_MOUSE_MOVED | EVENT_LEFT_DRAGGED | EVENT_RIGHT_DRAGGED | EVENT_OTHER_DRAGGED => {
            let dx = event_field(event, 4) as i32;
            let dy = event_field(event, 5) as i32;
            if dx != 0 || dy != 0 {
                let sequence = next_sequence(context);
                let timestamp_micros = SystemTime::UNIX_EPOCH
                    .elapsed()
                    .unwrap_or_default()
                    .as_micros() as u64;
                context.motion.send_replace(Some(Motion {
                    sequence,
                    timestamp_micros,
                    dx,
                    dy,
                }));
            }
        }
        EVENT_LEFT_DOWN => send_key(context, 272, 1),
        EVENT_LEFT_UP => send_key(context, 272, 0),
        EVENT_RIGHT_DOWN => send_key(context, 273, 1),
        EVENT_RIGHT_UP => send_key(context, 273, 0),
        EVENT_OTHER_DOWN | EVENT_OTHER_UP => {
            let button = match event_field(event, 3) {
                2 => Some(274),
                3 => Some(275),
                4 => Some(276),
                _ => None,
            };
            if let Some(button) = button {
                send_key(context, button, i32::from(event_type == EVENT_OTHER_DOWN));
            }
        }
        EVENT_KEY_DOWN | EVENT_KEY_UP => {
            let mac_code = event_field(event, 9) as u16;
            if let Some(code) = macos_key_to_linux(mac_code) {
                let value = if event_type == EVENT_KEY_UP {
                    0
                } else if event_field(event, 8) != 0 {
                    2
                } else {
                    1
                };
                send_key(context, code, value);
            }
        }
        EVENT_FLAGS_CHANGED => {
            let mac_code = event_field(event, 9) as u16;
            if let Some(code) = macos_key_to_linux(mac_code) {
                let down = modifier_flag(mac_code)
                    .is_some_and(|flag| unsafe { CGEventGetFlags(event) } & flag != 0);
                let value = {
                    let mut modifiers = context.modifiers.lock().unwrap();
                    if down {
                        modifiers.insert(code);
                        1
                    } else {
                        modifiers.remove(&code);
                        0
                    }
                };
                send_key(context, code, value);
            }
        }
        EVENT_SCROLL => {
            let vertical = event_field(event, 11) as i32;
            let horizontal = event_field(event, 12) as i32;
            if vertical != 0 {
                send_reliable(context, EV_REL, REL_WHEEL, vertical);
            }
            if horizontal != 0 {
                send_reliable(context, EV_REL, REL_HWHEEL, horizontal);
            }
        }
        _ => {}
    }
}

fn event_field(event: CGEventRef, field: u32) -> i64 {
    unsafe { CGEventGetIntegerValueField(event, field) }
}

fn next_sequence(context: &CaptureContext) -> u64 {
    context
        .sequence
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
}

fn send_key(context: &CaptureContext, code: u16, value: i32) {
    send_reliable(context, EV_KEY, code, value);
}

fn send_reliable(context: &CaptureContext, event_type: u16, code: u16, value: i32) {
    let event = ReliableEvent::Input {
        sequence: next_sequence(context),
        event_type,
        code,
        value,
    };
    if let Err(error) = context.reliable.blocking_send(event) {
        tracing::warn!(%error, "drop captured macOS input after transport closed");
    }
}

#[derive(Default)]
pub struct Injector {
    modifiers: HashSet<u16>,
}

impl Injector {
    pub fn new() -> Result<Self> {
        if !unsafe { AXIsProcessTrusted() } {
            tracing::warn!(
                "macOS does not report this process as Accessibility-trusted; input injection may be ignored"
            );
        }
        Ok(Self::default())
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

    pub fn set_cursor_position(&mut self, x: i32, y: i32) -> Result<()> {
        enable_background_cursor();
        check_cg_error(
            unsafe { CGDisplayShowCursor(CGMainDisplayID()) },
            "show macOS cursor",
        )?;
        check_cg_error(
            unsafe { CGAssociateMouseAndMouseCursorPosition(true) },
            "associate macOS mouse and cursor",
        )?;
        check_cg_error(
            unsafe {
                CGWarpMouseCursorPosition(CGPoint {
                    x: x.max(0) as f64,
                    y: y.max(0) as f64,
                })
            },
            "warp macOS cursor",
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
        if value == 0 {
            self.modifiers.remove(&code);
        } else if is_linux_modifier(code) {
            self.modifiers.insert(code);
        }
        let event =
            unsafe { CGEventCreateKeyboardEvent(std::ptr::null_mut(), keycode, value != 0) };
        post_with_flags_and_release(
            event,
            modifier_flags(&self.modifiers),
            "create macOS keyboard event",
        )
    }

    fn emit_scroll(&mut self, vertical: i32, horizontal: i32) -> Result<()> {
        let event = unsafe {
            CGEventCreateScrollWheelEvent(std::ptr::null_mut(), 1, 2, vertical, horizontal)
        };
        post_with_flags_and_release(
            event,
            modifier_flags(&self.modifiers),
            "create macOS scroll event",
        )
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
        let event =
            unsafe { CGEventCreateMouseEvent(std::ptr::null_mut(), event_type, position, button) };
        post_with_flags_and_release(
            event,
            modifier_flags(&self.modifiers),
            "create macOS mouse event",
        )
    }
}

fn enable_background_cursor() {
    let key = unsafe {
        CFStringCreateWithCString(std::ptr::null(), c"SetsCursorInBackground".as_ptr(), 0)
    };
    if key.is_null() {
        tracing::warn!("failed to create macOS background-cursor property name");
        return;
    }
    let connection = unsafe { _CGSDefaultConnection() };
    let error = unsafe { CGSSetConnectionProperty(connection, connection, key, kCFBooleanTrue) };
    unsafe { CFRelease(key) };
    if error != 0 {
        tracing::warn!(error, "failed to enable macOS cursor in background process");
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

fn post_with_flags_and_release(event: CGEventRef, flags: u64, message: &str) -> Result<()> {
    if event.is_null() {
        bail!("{message}");
    }
    unsafe { CGEventSetFlags(event, flags) };
    post_and_release(event, message)
}

fn check_cg_error(error: i32, operation: &str) -> Result<()> {
    if error == 0 {
        Ok(())
    } else {
        bail!("{operation} failed with CGError {error}")
    }
}

fn modifier_flag(mac_code: u16) -> Option<u64> {
    match mac_code {
        54 | 55 => Some(FLAG_COMMAND),
        56 | 60 => Some(FLAG_SHIFT),
        57 => Some(FLAG_CAPS_LOCK),
        58 | 61 => Some(FLAG_OPTION),
        59 | 62 => Some(FLAG_CONTROL),
        _ => None,
    }
}

fn is_linux_modifier(code: u16) -> bool {
    matches!(code, 29 | 42 | 54 | 56 | 58 | 97 | 100 | 125 | 126)
}

fn modifier_flags(modifiers: &HashSet<u16>) -> u64 {
    let mut flags = 0;
    if modifiers.contains(&58) {
        flags |= FLAG_CAPS_LOCK;
    }
    if modifiers.contains(&42) || modifiers.contains(&54) {
        flags |= FLAG_SHIFT;
    }
    if modifiers.contains(&29) || modifiers.contains(&97) {
        flags |= FLAG_CONTROL;
    }
    if modifiers.contains(&56) || modifiers.contains(&100) {
        flags |= FLAG_OPTION;
    }
    if modifiers.contains(&125) || modifiers.contains(&126) {
        flags |= FLAG_COMMAND;
    }
    flags
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
        125 => 55,
        126 => 54,
        _ => return None,
    })
}

fn macos_key_to_linux(code: u16) -> Option<u16> {
    Some(match code {
        0 => 30,
        1 => 31,
        2 => 32,
        3 => 33,
        4 => 35,
        5 => 34,
        6 => 44,
        7 => 45,
        8 => 46,
        9 => 47,
        11 => 48,
        12 => 16,
        13 => 17,
        14 => 18,
        15 => 19,
        16 => 21,
        17 => 20,
        18 => 2,
        19 => 3,
        20 => 4,
        21 => 5,
        22 => 7,
        23 => 6,
        24 => 13,
        25 => 10,
        26 => 8,
        27 => 12,
        28 => 9,
        29 => 11,
        30 => 27,
        31 => 24,
        32 => 22,
        33 => 26,
        34 => 23,
        35 => 25,
        36 => 28,
        37 => 38,
        38 => 36,
        39 => 40,
        40 => 37,
        41 => 39,
        42 => 43,
        43 => 51,
        44 => 53,
        45 => 49,
        46 => 50,
        47 => 52,
        48 => 15,
        49 => 57,
        50 => 41,
        51 => 14,
        53 => 1,
        55 => 125,
        56 => 42,
        57 => 58,
        58 => 56,
        59 => 29,
        60 => 54,
        61 => 100,
        62 => 97,
        96 => 63,
        97 => 64,
        98 => 65,
        99 => 61,
        100 => 66,
        101 => 67,
        103 => 87,
        109 => 68,
        111 => 88,
        114 => 110,
        115 => 102,
        116 => 104,
        117 => 111,
        118 => 62,
        119 => 107,
        120 => 60,
        121 => 109,
        122 => 59,
        123 => 105,
        124 => 106,
        125 => 108,
        126 => 103,
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
        assert_eq!(linux_key_to_macos(125), Some(55));
        assert_eq!(linux_key_to_macos(126), Some(54));
    }

    #[test]
    fn maps_common_macos_keys() {
        assert_eq!(macos_key_to_linux(0), Some(30));
        assert_eq!(macos_key_to_linux(36), Some(28));
        assert_eq!(macos_key_to_linux(126), Some(103));
        assert_eq!(macos_key_to_linux(700), None);
    }

    #[test]
    fn builds_flags_from_left_and_right_modifiers() {
        let modifiers = HashSet::from([42, 97, 100, 126]);
        assert_eq!(
            modifier_flags(&modifiers),
            FLAG_SHIFT | FLAG_CONTROL | FLAG_OPTION | FLAG_COMMAND
        );
    }

    #[test]
    fn maps_macos_modifier_keys_to_their_flag_groups() {
        assert_eq!(modifier_flag(54), Some(FLAG_COMMAND));
        assert_eq!(modifier_flag(60), Some(FLAG_SHIFT));
        assert_eq!(modifier_flag(61), Some(FLAG_OPTION));
        assert_eq!(modifier_flag(62), Some(FLAG_CONTROL));
        assert_eq!(modifier_flag(0), None);
    }
}
