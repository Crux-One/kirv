#[cfg(not(target_os = "macos"))]
compile_error!("kirv is macOS-only because it controls Darwin process groups.");

#[cfg(target_os = "macos")]
pub mod control;
#[cfg(target_os = "macos")]
pub mod service;
