pub mod core;
pub mod identity;
#[cfg(target_os = "linux")]
pub(crate) mod input;
#[cfg(target_os = "windows")]
#[path = "input_windows.rs"]
pub(crate) mod input;
#[cfg(target_os = "macos")]
#[path = "input_macos.rs"]
pub(crate) mod input;
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
compile_error!("rflow currently supports Linux, Windows, and macOS");
pub mod pairing;
pub mod platform;
pub mod protocol;
pub mod runtime;
pub mod target;
pub mod transport;
