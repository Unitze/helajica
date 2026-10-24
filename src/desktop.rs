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

use cli::Cli;

const ARGS: [&str; 2] = ["maki", "gui"];
/// Read by `cmd::tui`, which this binary shares with `maki`.
const TELEMETRY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

fn main() {
    color_eyre::install().ok();
    let args = ARGS
        .map(String::from)
        .into_iter()
        .chain(std::env::args().skip(1));
    let cli = Cli::parse_from(args);
    let result = cmd::dispatch(cli);
    maki_otel::shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
    if let Err(e) = result {
        tracing::error!(error = %e, "desktop app failed");
    }
}
