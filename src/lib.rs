#[cfg(target_os = "linux")]
pub mod input;
#[cfg(target_os = "windows")]
#[path = "input_windows.rs"]
pub mod input;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
compile_error!("rflow currently supports Linux and Windows");
pub mod protocol;
pub mod state;
pub mod transport;
