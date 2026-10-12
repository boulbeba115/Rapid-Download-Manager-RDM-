//! `rdm.exe` — Rapid Download Manager, desktop application.

// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use rdm_app::app::{Launch, RdmApp};

fn parse_args() -> Launch {
    let mut launch = Launch::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--data-dir" => launch.data_dir = it.next().map(PathBuf::from),
            "--screenshot" => launch.screenshot = it.next().map(PathBuf::from),
            "--show" => launch.show = it.next(),
            // Start in the notification area (used by "start with Windows").
            "--minimized" => launch.minimized = true,
            _ => launch.args.push(arg),
        }
    }
    launch
}

/// Settings live next to the executable when a `portable.txt` marker (or an
/// existing `data` folder) is there, otherwise in `%APPDATA%\RDM`.
fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)) {
        if dir.join("portable.txt").exists() || dir.join("data").join("config.json").exists() {
            return dir.join("data");
        }
    }
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("RDM")
}

fn main() -> eframe::Result {
    let launch = parse_args();
    let data_dir = launch.data_dir.clone().unwrap_or_else(default_data_dir);

    // Single instance: if RDM is already running, hand it our arguments and leave.
    // (Screenshot runs use their own data directory and are always separate.)
    if launch.screenshot.is_none() {
        let port = rdm_core::state::load_json::<rdm_core::Config>(&data_dir.join("config.json")).map(|c| c.api_port).unwrap_or(rdm_core::manager::DEFAULT_API_PORT);
        if rdm_core::api::forward_to_running(port, &launch.args) {
            return Ok(());
        }
    }
    rdm_app::logging::init(&data_dir);
    log::info!("RDM {} starting, data in {}", env!("CARGO_PKG_VERSION"), data_dir.display());

    let icon = egui::IconData { rgba: rdm_app::icon::render(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rapid Download Manager")
            .with_inner_size([1120.0, 640.0])
            .with_min_inner_size([680.0, 380.0])
            .with_visible(!launch.minimized)
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    eframe::run_native(
        "Rapid Download Manager",
        options,
        Box::new(move |cc| Ok(Box::new(RdmApp::new(cc, data_dir, launch)?))),
    )
}
