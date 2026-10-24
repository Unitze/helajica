//! The desktop app as its own executable: double-click it, no terminal.
//! Shares every module with `maki`, and runs exactly what `maki gui` runs.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod cli;
mod cmd;
mod print;
mod project_trust;
mod provider_scripts;
mod resume;
mod sdk_mode;
mod setup;
mod update;

use std::time::Duration;

use clap::Parser;
use maki_config::providers::ProvidersConfig;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MessageBoxW};

use cli::Cli;

const ARGS: [&str; 2] = ["maki", "gui"];
/// Read by `cmd::tui`, which this binary shares with `maki`.
const TELEMETRY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const ALERT_TITLE: &str = "Helajica";

fn main() {
    color_eyre::install().ok();
    let args = ARGS
        .map(String::from)
        .into_iter()
        .chain(std::env::args().skip(1));
    let cli = Cli::parse_from(args);
    // `ProvidersConfig::load` exits on a broken file, and with no console that
    // reads as the window vanishing the moment it opens.
    if let Some((aside, error)) = ProvidersConfig::set_aside_if_broken() {
        alert(&format!(
            "providers.toml could not be read, so it was moved to
{}

Helajica starts without your custom providers. Fix the file and move it back to restore them.

{error}",
            aside.display()
        ));
    }
    let result = cmd::dispatch(cli);
    maki_otel::shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
    if let Err(e) = result {
        tracing::error!(error = %e, "desktop app failed");
    }
}

#[cfg(windows)]
fn alert(message: &str) {
    let wide = |text: &str| text.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (message, title) = (wide(message), wide(ALERT_TITLE));
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONWARNING,
        )
    };
}

#[cfg(not(windows))]
fn alert(message: &str) {
    eprintln!("{ALERT_TITLE}: {message}");
}
