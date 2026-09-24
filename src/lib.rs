pub mod android;
pub mod app_protocol;
#[cfg(feature = "cdp")]
pub mod cdp;
pub mod cli;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod server;
pub mod tools;
#[cfg(target_os = "windows")]
pub mod windows;

// Re-export platform module as `platform` for unified access
#[cfg(target_os = "macos")]
pub use macos as platform;
#[cfg(target_os = "windows")]
pub use windows as platform;
