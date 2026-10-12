//! `rdm.exe` — Rapid Download Manager, desktop application.

// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod dialogs;
mod progress;
mod theme;
mod widgets;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

use app::Launch;

fn parse_args() -> Launch {
    let mut launch = Launch::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--data-dir" => launch.data_dir = it.next().map(PathBuf::from),
            "--screenshot" => launch.screenshot = it.next().map(PathBuf::from),
            "--show" => launch.show = it.next(),
            _ => launch.args.push(arg),
        }
    }
    launch
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("RDM")
}

/// The window icon: a white download arrow on a blue rounded square.
fn icon() -> egui::IconData {
    const N: usize = 64;
    let mut rgba = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded square.
            let r = 12.0;
            let (cx, cy) = (fx.clamp(r, N as f32 - r), fy.clamp(r, N as f32 - r));
            let inside = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() <= r;
            if !inside {
                continue;
            }
            // Arrow: shaft, head, and a tray line underneath.
            let shaft = (26.0..=38.0).contains(&fx) && (12.0..=32.0).contains(&fy);
            let head = (30.0..=46.0).contains(&fy) && (fx - 32.0).abs() <= (46.0 - fy);
            let tray = (14.0..=50.0).contains(&fx) && (50.0..=55.0).contains(&fy);
            let px = if shaft || head || tray { [255, 255, 255, 255] } else { [0x1E, 0x7F, 0xD6, 255] };
            rgba[(y * N + x) * 4..(y * N + x) * 4 + 4].copy_from_slice(&px);
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rapid Download Manager")
            .with_inner_size([1120.0, 640.0])
            .with_min_inner_size([680.0, 380.0])
            .with_icon(Arc::new(icon())),
        ..Default::default()
    };
    eframe::run_native(
        "Rapid Download Manager",
        options,
        Box::new(move |cc| Ok(Box::new(app::RdmApp::new(cc, data_dir, launch)?))),
    )
}
