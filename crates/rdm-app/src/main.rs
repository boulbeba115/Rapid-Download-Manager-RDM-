//! `rdm.exe` — Rapid Download Manager, desktop application.

// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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

/// Become the running instance, or hand our arguments to the one that already is.
///
/// Holding the loopback port is what makes an instance *the* instance, so the
/// port is taken here, before any window exists. `None` means another
/// instance took our arguments and this process should exit.
fn claim_instance(data_dir: &Path, args: &[String]) -> Option<Option<TcpListener>> {
    let port = rdm_core::state::load_json::<rdm_core::Config>(&data_dir.join("config.json")).map(|c| c.api_port).unwrap_or(rdm_core::manager::DEFAULT_API_PORT);
    // Two instances started at the same moment (the installer's last page can do
    // that): the loser of the race for the port waits for the winner to listen.
    for _ in 0..20 {
        if rdm_core::api::forward_to_running(port, data_dir, args) {
            return None;
        }
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Some(Some(listener)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => std::thread::sleep(Duration::from_millis(150)),
            Err(_) => break,
        }
    }
    // The port belongs to something else: run anyway, without browser integration.
    Some(None)
}

fn main() -> eframe::Result {
    let mut launch = parse_args();
    let portable = rdm_core::state::portable_data_dir();
    let data_dir = launch.data_dir.clone().unwrap_or_else(rdm_core::state::default_data_dir);
    // A portable copy must keep working when its folder is carried to another PC,
    // so its saved passwords are not tied to this Windows account.
    rdm_core::security::set_protect_at_rest(portable.as_deref() != Some(data_dir.as_path()));

    // Single instance. (Screenshot runs use their own data directory and are always separate.)
    if launch.screenshot.is_none() {
        match claim_instance(&data_dir, &launch.args) {
            None => return Ok(()),
            Some(listener) => launch.listener = listener.map(Arc::new),
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
