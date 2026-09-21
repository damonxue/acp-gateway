//! Native desktop companion for Agent Gateway.

#[cfg(target_os = "macos")]
mod macos;

#[cfg(windows)]
mod windows_app;

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(windows)]
fn main() {
    windows_app::run();
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("agent-gateway-app supports macOS and Windows");
}
