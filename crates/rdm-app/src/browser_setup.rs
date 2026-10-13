//! "Browser integration": whether the browsers know about RDM, and how to
//! add the extension.

use std::path::PathBuf;

use eframe::egui::{self, Align2, RichText};
use rdm_core::browser;

use crate::theme::{GREEN, ORANGE, RED};

pub struct BrowserSetupDialog {
    /// Folder holding `rdm.exe`, `rdm-host.exe` and the `extension` folder.
    install_dir: PathBuf,
    /// The host program the browsers are set to start, if any.
    registered: Option<PathBuf>,
    /// Outcome of the last button press: `(text, is_error)`.
    message: Option<(String, bool)>,
}

impl Default for BrowserSetupDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserSetupDialog {
    pub fn new() -> Self {
        let install_dir = std::env::current_exe().ok().and_then(|exe| exe.parent().map(PathBuf::from)).unwrap_or_default();
        Self { install_dir, registered: browser::registered_host(), message: None }
    }

    fn extension_dir(&self) -> PathBuf {
        self.install_dir.join("extension")
    }

    fn register(&mut self) {
        self.message = Some(match browser::register(&self.install_dir) {
            Ok(_) => ("Connected. Restart the browser if the extension is already installed.".into(), false),
            Err(e) => (format!("Could not connect: {e}"), true),
        });
        self.registered = browser::registered_host();
    }

    /// Returns false once closed.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        let mut open = true;
        let mut close = false;
        egui::Window::new("Browser integration").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, -30.0]).open(&mut open).show(ctx, |ui| {
            ui.set_width(560.0);

            ui.label(RichText::new("1.  Connection between the browsers and RDM").strong());
            let ours = self.install_dir.join(browser::HOST_EXE);
            match &self.registered {
                Some(host) if *host == ours => {
                    ui.label(RichText::new("Connected: Chrome, Edge, Brave and Chromium start this copy of RDM.").color(GREEN));
                }
                Some(host) => {
                    ui.label(RichText::new(format!("The browsers are connected to another copy of RDM:\n{}", host.display())).color(ORANGE));
                    if ui.button("Connect them to this copy").clicked() {
                        self.register();
                    }
                }
                None => {
                    ui.label(RichText::new("Not connected: the extension cannot reach RDM yet.").color(RED));
                    if ui.button("Connect the browsers to RDM").clicked() {
                        self.register();
                    }
                }
            }
            ui.label(
                RichText::new(
                    "The installer does this for you. It registers RDM's helper with the browser so that RDM's own extension, \
                     and nothing else, can hand it downloads. No administrator rights are involved.",
                )
                .weak(),
            );
            if let Some((text, is_error)) = &self.message {
                ui.label(RichText::new(text).color(if *is_error { RED } else { GREEN }));
            }

            ui.add_space(10.0);
            ui.label(RichText::new("2.  Add the extension to your browser").strong());
            ui.label("Open the browser's extensions page:");
            ui.horizontal(|ui| {
                if ui.button("Open in Chrome").clicked() {
                    let _ = open::with_detached("chrome://extensions/", "chrome");
                }
                if ui.button("Open in Edge").clicked() {
                    let _ = open::with_detached("edge://extensions/", "msedge");
                }
                ui.label(RichText::new("or type chrome://extensions in the address bar").weak());
            });
            ui.label("Switch on \"Developer mode\", click \"Load unpacked\" and choose this folder:");
            let folder = self.extension_dir();
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(folder.display().to_string()).monospace()).truncate());
            });
            ui.horizontal(|ui| {
                if ui.button("Copy the folder path").clicked() {
                    ctx.copy_text(folder.display().to_string());
                    self.message = Some(("Folder path copied.".into(), false));
                }
                if ui.button("Show the folder").clicked() {
                    let _ = open::that_detached(&folder);
                }
            });
            ui.label(
                RichText::new(
                    "Chrome and Edge only install extensions by themselves from their own stores, so this one-time step \
                     is needed for as long as the extension is not published there.",
                )
                .weak(),
            );

            ui.add_space(10.0);
            ui.label(RichText::new("3.  Use it").strong());
            ui.label(
                "Downloads of the file types listed under Options, File types go to RDM instead of the browser. \
                 Videos get a \"Download with RDM\" button, and the extension's toolbar icon lists the media found on the page.",
            );

            ui.add_space(8.0);
            ui.separator();
            if ui.add_sized([90.0, 24.0], egui::Button::new("Close")).clicked() {
                close = true;
            }
        });
        open && !close
    }
}
