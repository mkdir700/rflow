#[cfg(target_os = "linux")]
pub mod input;
#[cfg(target_os = "windows")]
#[path = "input_windows.rs"]
pub mod input;
#[cfg(target_os = "macos")]
#[path = "input_macos.rs"]
pub mod input;
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
compile_error!("rflow currently supports Linux, Windows, and macOS");
pub mod protocol;
pub mod state;
pub mod transport;
