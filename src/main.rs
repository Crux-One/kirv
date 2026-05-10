#[cfg(not(target_os = "macos"))]
compile_error!("kirv is macOS-only because it controls Darwin process groups.");

#[cfg(target_os = "macos")]
use kirv::service;

#[cfg(target_os = "macos")]
fn main() {
    if let Err(err) = service::run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
