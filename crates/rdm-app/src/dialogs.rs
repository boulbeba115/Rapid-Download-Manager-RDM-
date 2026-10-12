//! Modal-style dialogs shown inside the main window.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, RichText};
use rdm_core::http::{ProxyConfig, RemoteInfo, RequestSpec};
use rdm_core::manager::{AddRequest, PostAction, ProbeTicket, SiteLimit, SiteLogin, MAIN_QUEUE};
use rdm_core::task::Connections;
use rdm_core::util::format_size;
use rdm_core::{Config, Id, Manager, NewDownload, Queue, Status, View};

use crate::theme::{GREEN, RED};

fn dialog(title: &str) -> egui::Window<'_> {
    egui::Window::new(title).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, -30.0])
}

fn wide_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_sized([110.0, 24.0], egui::Button::new(text))
}

// ---------------------------------------------------------------- Add URL

enum Stage {
    Url,
    Probing,
    Info,
    Failed(String),
}

pub enum AddOutcome {
    Keep,
    Close,
    Added { id: Id, started: bool, quit_when_done: bool },
}

pub struct AddDialog {
    stage: Stage,
    url: String,
    use_auth: bool,
    user: String,
    password: String,
    /// Context supplied by the browser (referer, cookies, its User-Agent).
    request: AddRequest,
    ticket: Option<ProbeTicket>,
    info: Option<RemoteInfo>,
    category: String,
    save_as: String,
    description: String,
    first_frame: bool,
    checksum: String,
    /// The address is an HLS playlist rather than a file.
    is_stream: bool,
}

impl AddDialog {
    pub fn new(url: String) -> Self {
        Self {
            stage: Stage::Url,
            url,
            use_auth: false,
            user: String::new(),
            password: String::new(),
            request: AddRequest::default(),
            ticket: None,
            info: None,
            category: String::new(),
            save_as: String::new(),
            description: String::new(),
            first_frame: true,
            checksum: String::new(),
            is_stream: false,
        }
    }

    /// A link handed over by the browser or the command line: skip straight to probing.
    pub fn from_request(request: AddRequest, manager: &Manager) -> Self {
        let mut d = Self::new(request.url.clone());
        d.request = request;
        d.begin_probe(manager);
        d
    }

    fn spec(&self) -> RequestSpec {
        RequestSpec {
            url: self.url.trim().to_string(),
            referer: self.request.referer.clone(),
            cookies: self.request.cookies.clone(),
            user_agent: self.request.user_agent.clone(),
            auth: (self.use_auth && !self.user.is_empty()).then(|| (self.user.clone(), self.password.clone())),
            headers: Vec::new(),
        }
    }

    fn begin_probe(&mut self, manager: &Manager) {
        self.ticket = Some(manager.probe(self.spec()));
        self.info = None;
        self.stage = Stage::Probing;
    }

    fn url_ok(&self) -> bool {
        let u = self.url.trim();
        u.starts_with("http://") || u.starts_with("https://")
    }

    fn place(&mut self, manager: &Manager, name: &str) {
        let (category, dir) = manager.default_placement(name);
        let dir = self.request.dir.clone().unwrap_or(dir);
        self.category = category;
        self.save_as = dir.join(name).display().to_string();
    }

    fn submit(&mut self, manager: &Manager, start: bool) -> AddOutcome {
        let path = PathBuf::from(self.save_as.trim());
        let mut new = NewDownload::new(self.spec());
        new.file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty());
        new.dir = path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf);
        new.category = (!self.category.is_empty()).then(|| self.category.clone());
        new.description = self.description.clone();
        new.expected_hash = Some(self.checksum.trim().to_string()).filter(|c| !c.is_empty());
        new.start = start;
        new.queue = (!start).then_some(MAIN_QUEUE);
        // A stream's first response is its playlist, not file data worth reusing.
        new.probe = if start && !self.is_stream { self.ticket.as_ref().and_then(|t| t.take()) } else { None };
        AddOutcome::Added { id: manager.add(new), started: start, quit_when_done: self.request.quit_when_done }
    }

    pub fn show(&mut self, ctx: &egui::Context, manager: &Manager, config: &Config) -> AddOutcome {
        // Advance the probe.
        if matches!(self.stage, Stage::Probing) {
            match self.ticket.as_ref().and_then(|t| t.result()) {
                Some(Ok(info)) => {
                    self.is_stream = rdm_core::hls::looks_like_hls(self.url.trim(), info.content_type.as_deref());
                    let name = if self.is_stream {
                        // Name the video, not the playlist ("master.m3u8").
                        format!("{}.ts", rdm_core::hls::stream_name(self.url.trim()))
                    } else {
                        self.request.file_name.clone().unwrap_or_else(|| info.file_name.clone())
                    };
                    self.place(manager, &name);
                    self.info = Some(info);
                    self.stage = Stage::Info;
                }
                Some(Err(e)) => self.stage = Stage::Failed(e),
                None => ctx.request_repaint_after(Duration::from_millis(100)),
            }
        }

        let mut outcome = AddOutcome::Keep;
        let mut open = true;
        let title = match self.stage {
            Stage::Url => "Enter new address to download",
            _ => "Download File Info",
        };
        dialog(title).open(&mut open).show(ctx, |ui| {
            ui.set_width(520.0);
            match &self.stage {
                Stage::Url => {
                    ui.horizontal(|ui| {
                        ui.label("Address");
                        let edit = ui.add(egui::TextEdit::singleline(&mut self.url).desired_width(f32::INFINITY).hint_text("https://…"));
                        if self.first_frame {
                            edit.request_focus();
                        }
                        if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && self.url_ok() {
                            self.begin_probe(manager);
                        }
                    });
                    ui.add_space(4.0);
                    ui.checkbox(&mut self.use_auth, "Use authorization");
                    if self.use_auth {
                        egui::Grid::new("auth").num_columns(2).show(ui, |ui| {
                            ui.label("Login");
                            ui.add(egui::TextEdit::singleline(&mut self.user).desired_width(200.0));
                            ui.end_row();
                            ui.label("Password");
                            ui.add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(200.0));
                            ui.end_row();
                        });
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(self.url_ok(), egui::Button::new("OK").min_size([90.0, 24.0].into())).clicked() {
                            self.begin_probe(manager);
                        }
                        if ui.add_sized([90.0, 24.0], egui::Button::new("Cancel")).clicked() {
                            outcome = AddOutcome::Close;
                        }
                    });
                }
                Stage::Probing => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Getting file information…");
                    });
                    ui.add(egui::Label::new(RichText::new(self.url.trim()).weak()).truncate());
                    ui.add_space(8.0);
                    if ui.add_sized([90.0, 24.0], egui::Button::new("Cancel")).clicked() {
                        outcome = AddOutcome::Close;
                    }
                }
                Stage::Failed(err) => {
                    ui.label(RichText::new("Could not get the file information").strong());
                    ui.label(RichText::new(err).color(RED));
                    ui.add(egui::Label::new(RichText::new(self.url.trim()).weak()).truncate());
                    ui.add_space(8.0);
                    let mut retry = false;
                    let mut anyway = false;
                    ui.horizontal(|ui| {
                        retry = wide_button(ui, "Try again").clicked();
                        anyway = wide_button(ui, "Add anyway").on_hover_text("Add to the list; RDM keeps retrying when started").clicked();
                        if ui.add_sized([90.0, 24.0], egui::Button::new("Cancel")).clicked() {
                            outcome = AddOutcome::Close;
                        }
                    });
                    if retry {
                        self.begin_probe(manager);
                    } else if anyway {
                        let mut new = NewDownload::new(self.spec());
                        new.file_name = self.request.file_name.clone();
                        new.dir = self.request.dir.clone();
                        new.start = false;
                        new.queue = Some(MAIN_QUEUE);
                        outcome = AddOutcome::Added { id: manager.add(new), started: false, quit_when_done: false };
                    }
                }
                Stage::Info => {
                    let mut category_changed = false;
                    egui::Grid::new("info").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        ui.label("URL");
                        ui.add(egui::Label::new(self.url.trim()).truncate());
                        ui.end_row();

                        ui.label("Category");
                        egui::ComboBox::from_id_salt("category").width(180.0).selected_text(self.category.clone()).show_ui(ui, |ui| {
                            for c in &config.categories {
                                category_changed |= ui.selectable_value(&mut self.category, c.name.clone(), c.name.as_str()).changed();
                            }
                        });
                        ui.end_row();

                        ui.label("Save As");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.save_as).desired_width(400.0));
                            if ui.button("…").on_hover_text("Browse").clicked() {
                                let current = PathBuf::from(self.save_as.trim());
                                let mut picker = rfd::FileDialog::new();
                                if let Some(dir) = current.parent() {
                                    picker = picker.set_directory(dir);
                                }
                                if let Some(name) = current.file_name() {
                                    picker = picker.set_file_name(name.to_string_lossy());
                                }
                                if let Some(chosen) = picker.save_file() {
                                    self.save_as = chosen.display().to_string();
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Description");
                        ui.add(egui::TextEdit::singleline(&mut self.description).desired_width(400.0));
                        ui.end_row();

                        ui.label("Checksum");
                        ui.add(egui::TextEdit::singleline(&mut self.checksum).desired_width(400.0).hint_text("optional — MD5, SHA-1 or SHA-256 to verify the file"));
                        ui.end_row();
                    });
                    if category_changed {
                        // Move the file to the newly chosen category's folder, keeping its name.
                        if let Some(dir) = rdm_core::category::dir_for(&self.category, &config.categories) {
                            let name = PathBuf::from(self.save_as.trim()).file_name().map(|n| n.to_os_string());
                            if let Some(name) = name {
                                self.save_as = dir.join(name).display().to_string();
                            }
                        }
                    }
                    let checksum = self.checksum.trim();
                    if !checksum.is_empty() && rdm_core::checksum::parse(checksum).is_none() {
                        ui.label(RichText::new("The checksum is not a valid MD5, SHA-1 or SHA-256 value.").color(RED));
                    }
                    if self.is_stream {
                        ui.add_space(4.0);
                        ui.label(RichText::new("Video stream (HLS) — the best quality is downloaded and joined into one file").color(GREEN));
                    } else if let Some(info) = &self.info {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(info.size.map(format_size).unwrap_or_else(|| "Size unknown".into())).strong());
                            ui.separator();
                            if info.resumable {
                                ui.label(RichText::new("Resumable").color(GREEN));
                            } else {
                                ui.label(RichText::new("Not resumable — single connection").color(RED));
                            }
                        });
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let valid = PathBuf::from(self.save_as.trim()).file_name().is_some();
                        let later = ui.add_enabled(valid, egui::Button::new("Download Later").min_size([120.0, 26.0].into())).clicked();
                        let now = ui.add_enabled(valid, egui::Button::new(RichText::new("Start Download").strong()).min_size([130.0, 26.0].into())).clicked();
                        if ui.add_sized([90.0, 26.0], egui::Button::new("Cancel")).clicked() {
                            outcome = AddOutcome::Close;
                        }
                        if now || later {
                            // Record how the choice was made: a download must never start without the user.
                            let by = ui.input(|i| if i.pointer.primary_released() { "mouse click" } else { "keyboard" });
                            log::info!("{} confirmed by {by}: {}", if now { "start" } else { "download later" }, self.url.trim());
                        }
                        if now {
                            outcome = self.submit(manager, true);
                        } else if later {
                            outcome = self.submit(manager, false);
                        }
                    });
                }
            }
        });
        self.first_frame = false;
        if !open {
            outcome = AddOutcome::Close;
        }
        outcome
    }
}

// ---------------------------------------------------------------- Delete

pub struct DeleteDialog {
    pub ids: Vec<Id>,
    pub any_complete: bool,
    pub delete_files: bool,
}

impl DeleteDialog {
    /// `Some(true)` = confirmed, `Some(false)` = cancelled, `None` = still open.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<bool> {
        let mut result = None;
        let modal = egui::Modal::new(egui::Id::new("delete-confirm")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new("Confirm removal").heading());
            ui.add_space(4.0);
            ui.label(match self.ids.len() {
                1 => "Remove the selected download from the list?".to_string(),
                n => format!("Remove the {n} selected downloads from the list?"),
            });
            ui.label(RichText::new("Partially downloaded data is deleted.").weak());
            if self.any_complete {
                ui.checkbox(&mut self.delete_files, "Also delete the completed files from disk");
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_sized([90.0, 24.0], egui::Button::new("Remove")).clicked() {
                    result = Some(true);
                }
                if ui.add_sized([90.0, 24.0], egui::Button::new("Cancel")).clicked() {
                    result = Some(false);
                }
            });
        });
        if modal.should_close() && result.is_none() {
            result = Some(false);
        }
        result
    }
}

// ---------------------------------------------------------------- Properties

pub struct PropertiesDialog {
    pub id: Id,
    url: String,
    description: String,
}

impl PropertiesDialog {
    pub fn new(v: &View) -> Self {
        Self { id: v.d.id, url: v.d.spec.url.clone(), description: v.d.description.clone() }
    }

    pub fn show(&mut self, ctx: &egui::Context, manager: &Manager) -> bool {
        let Some(v) = manager.view(self.id) else { return false };
        let mut open = true;
        let mut close = false;
        dialog("Download properties").open(&mut open).show(ctx, |ui| {
            ui.set_width(520.0);
            let running = v.d.status == Status::Running;
            egui::Grid::new("props").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("File");
                ui.add(egui::Label::new(RichText::new(&v.d.file_name).strong()).truncate());
                ui.end_row();
                ui.label("Status");
                let (text, color) = crate::progress::status_text(&v);
                ui.add(egui::Label::new(RichText::new(text).color(color)).truncate());
                ui.end_row();
                ui.label("Size");
                ui.label(match v.d.size {
                    Some(s) => format!("{} of {}", format_size(v.d.downloaded), format_size(s)),
                    None => format_size(v.d.downloaded),
                });
                ui.end_row();
                ui.label("Save to");
                ui.add(egui::Label::new(v.d.path().display().to_string()).truncate());
                ui.end_row();
                ui.label("Address");
                ui.add_enabled(!running, egui::TextEdit::singleline(&mut self.url).desired_width(420.0))
                    .on_hover_text("If the link has expired, paste the new address here; progress is kept");
                ui.end_row();
                ui.label("Description");
                ui.add(egui::TextEdit::singleline(&mut self.description).desired_width(420.0));
                ui.end_row();
                ui.label("Added");
                ui.label(v.d.added_at.format("%Y-%m-%d %H:%M").to_string());
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_sized([90.0, 24.0], egui::Button::new("OK")).clicked() {
                    let url = self.url.trim().to_string();
                    if url != v.d.spec.url && (url.starts_with("http://") || url.starts_with("https://")) {
                        manager.set_url(self.id, url);
                    }
                    if self.description != v.d.description {
                        manager.set_description(self.id, self.description.clone());
                    }
                    close = true;
                }
                if ui.add_sized([90.0, 24.0], egui::Button::new("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        open && !close
    }
}

// ---------------------------------------------------------------- Batch

pub struct BatchDialog {
    template: String,
    from: u32,
    to: u32,
    digits: u32,
    start_now: bool,
}

impl BatchDialog {
    pub fn new() -> Self {
        Self { template: "https://example.com/files/part*.zip".into(), from: 1, to: 10, digits: 2, start_now: false }
    }

    fn url(&self, n: u32) -> String {
        rdm_core::util::expand_template(&self.template, n, n, self.digits as usize, 1).pop().unwrap_or_default()
    }

    pub fn show(&mut self, ctx: &egui::Context, manager: &Manager) -> bool {
        let mut open = true;
        let mut close = false;
        dialog("Batch download").open(&mut open).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label("Address template — use * where the number goes:");
            ui.add(egui::TextEdit::singleline(&mut self.template).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                ui.label("From");
                ui.add(egui::DragValue::new(&mut self.from).range(0..=99_999));
                ui.label("to");
                ui.add(egui::DragValue::new(&mut self.to).range(0..=99_999));
                ui.label("digits");
                ui.add(egui::DragValue::new(&mut self.digits).range(1..=8));
            });
            let ok = self.template.contains('*') && self.to >= self.from && (self.template.starts_with("http://") || self.template.starts_with("https://"));
            let count = if ok { (self.to - self.from + 1).min(5000) } else { 0 };
            ui.add_space(4.0);
            if ok {
                ui.label(RichText::new(format!("First: {}", self.url(self.from))).weak());
                ui.label(RichText::new(format!("Last:  {}", self.url(self.to))).weak());
            } else {
                ui.label(RichText::new("Enter an http(s) address containing * and a valid range.").color(RED));
            }
            ui.checkbox(&mut self.start_now, "Start downloading immediately (otherwise add to the main queue)");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(ok, egui::Button::new(format!("Add {count} downloads")).min_size([150.0, 24.0].into())).clicked() {
                    for n in self.from..self.from + count {
                        let mut new = NewDownload::new(RequestSpec::new(self.url(n)));
                        new.start = self.start_now;
                        new.queue = (!self.start_now).then_some(MAIN_QUEUE);
                        manager.add(new);
                    }
                    close = true;
                }
                if ui.add_sized([90.0, 24.0], egui::Button::new("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        open && !close
    }
}

// ---------------------------------------------------------------- Options

#[derive(PartialEq, Clone, Copy)]
enum OptTab {
    General,
    FileTypes,
    SaveTo,
    Downloads,
    Connection,
    Proxy,
    Logins,
}

pub struct OptionsDialog {
    cfg: Config,
    tab: OptTab,
    extensions: String,
    limit_kb: u64,
    proxy_mode: u8,
    proxy_url: String,
    proxy_user: String,
    proxy_pass: String,
    new_login: SiteLogin,
    new_limit: SiteLimit,
    error: Option<String>,
    pub api_status: String,
}

const CONNECTION_CHOICES: [u32; 7] = [1, 2, 4, 8, 16, 24, 32];

impl OptionsDialog {
    pub fn new(cfg: Config, api_status: String) -> Self {
        let (proxy_mode, proxy_url, proxy_user, proxy_pass) = match &cfg.net.proxy {
            ProxyConfig::System => (0, String::new(), String::new(), String::new()),
            ProxyConfig::None => (1, String::new(), String::new(), String::new()),
            ProxyConfig::Manual { url, username, password } => (2, url.clone(), username.clone(), password.clone()),
        };
        Self {
            extensions: cfg.capture_extensions.join(" "),
            limit_kb: (cfg.speed_limit / 1024).max(1),
            proxy_mode,
            proxy_url,
            proxy_user,
            proxy_pass,
            new_login: SiteLogin { host: String::new(), user: String::new(), password: String::new() },
            new_limit: SiteLimit { host: String::new(), connections: 1 },
            error: None,
            api_status,
            tab: OptTab::General,
            cfg,
        }
    }

    fn collect(&mut self) -> Config {
        let mut cfg = self.cfg.clone();
        cfg.capture_extensions = self
            .extensions
            .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        cfg.speed_limit = self.limit_kb * 1024;
        cfg.net.proxy = match self.proxy_mode {
            0 => ProxyConfig::System,
            1 => ProxyConfig::None,
            _ => ProxyConfig::Manual { url: self.proxy_url.trim().into(), username: self.proxy_user.clone(), password: self.proxy_pass.clone() },
        };
        cfg
    }

    /// Returns false once closed. `applied` is set when settings were saved.
    pub fn show(&mut self, ctx: &egui::Context, manager: &Manager, applied: &mut bool) -> bool {
        let mut open = true;
        let mut close = false;
        dialog("Options").open(&mut open).show(ctx, |ui| {
            ui.set_width(600.0);
            ui.horizontal_wrapped(|ui| {
                for (tab, name) in [
                    (OptTab::General, "General"),
                    (OptTab::FileTypes, "File types"),
                    (OptTab::SaveTo, "Save to"),
                    (OptTab::Downloads, "Downloads"),
                    (OptTab::Connection, "Connection"),
                    (OptTab::Proxy, "Proxy"),
                    (OptTab::Logins, "Site logins"),
                ] {
                    ui.selectable_value(&mut self.tab, tab, name);
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().max_height(330.0).min_scrolled_height(330.0).auto_shrink([false, false]).show(ui, |ui| match self.tab {
                OptTab::General => self.general(ui),
                OptTab::FileTypes => self.file_types(ui),
                OptTab::SaveTo => self.save_to(ui),
                OptTab::Downloads => self.downloads(ui),
                OptTab::Connection => self.connection(ui),
                OptTab::Proxy => self.proxy(ui),
                OptTab::Logins => self.logins(ui),
            });
            ui.separator();
            if let Some(err) = &self.error {
                ui.label(RichText::new(err).color(RED));
            }
            ui.horizontal(|ui| {
                if ui.add_sized([90.0, 24.0], egui::Button::new("OK")).clicked() {
                    match manager.set_config(self.collect()) {
                        Ok(()) => {
                            manager.save_now();
                            *applied = true;
                            close = true;
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                if ui.add_sized([90.0, 24.0], egui::Button::new("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        open && !close
    }

    fn general(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.cfg.monitor_clipboard, "Monitor the clipboard for download links");
        ui.checkbox(&mut self.cfg.close_to_tray, "Closing the window keeps RDM running in the notification area");
        ui.checkbox(&mut self.cfg.dark_mode, "Dark theme");
        ui.add_space(10.0);
        ui.label(RichText::new("Browser integration").strong());
        ui.horizontal(|ui| {
            ui.label("Local port");
            ui.add(egui::DragValue::new(&mut self.cfg.api_port).range(1024..=65535));
            ui.label(RichText::new("(restart RDM after changing)").weak());
        });
        ui.label(&self.api_status);
        ui.label(
            RichText::new(
                "Install the extension from the \"extension\" folder: open chrome://extensions (or edge://extensions), \
                 enable Developer mode, choose \"Load unpacked\" and select that folder.",
            )
            .weak(),
        );
    }

    fn file_types(&mut self, ui: &mut egui::Ui) {
        ui.label("Take over downloads of these file types from the browser, and react to them on the clipboard:");
        ui.add(egui::TextEdit::multiline(&mut self.extensions).desired_width(f32::INFINITY).desired_rows(8));
        ui.label(RichText::new("Separate extensions with spaces.").weak());
    }

    fn save_to(&mut self, ui: &mut egui::Ui) {
        ui.label("Each category saves to its own folder. Files are assigned by extension.");
        ui.add_space(4.0);
        let mut remove = None;
        for (i, cat) in self.cfg.categories.iter_mut().enumerate() {
            ui.push_id(i, |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized([110.0, 20.0], egui::Label::new(RichText::new(&cat.name).strong()));
                    let mut dir = cat.dir.display().to_string();
                    if ui.add(egui::TextEdit::singleline(&mut dir).desired_width(340.0)).changed() {
                        cat.dir = PathBuf::from(dir);
                    }
                    if ui.button("…").clicked() {
                        if let Some(chosen) = rfd::FileDialog::new().set_directory(&cat.dir).pick_folder() {
                            cat.dir = chosen;
                        }
                    }
                    if cat.name != rdm_core::category::GENERAL && ui.small_button("×").on_hover_text("Remove category").clicked() {
                        remove = Some(i);
                    }
                });
                if cat.name != rdm_core::category::GENERAL {
                    ui.horizontal(|ui| {
                        ui.add_space(118.0);
                        let mut exts = cat.extensions.join(" ");
                        if ui.add(egui::TextEdit::singleline(&mut exts).desired_width(340.0).hint_text("extensions")).changed() {
                            cat.extensions = exts.split_whitespace().map(|e| e.trim_start_matches('.').to_ascii_lowercase()).collect();
                        }
                    });
                }
            });
        }
        if let Some(i) = remove {
            self.cfg.categories.remove(i);
        }
        ui.add_space(6.0);
        if ui.button("Add category").clicked() {
            let base = rdm_core::category::dir_for(rdm_core::category::GENERAL, &self.cfg.categories).map(Path::to_path_buf).unwrap_or_default();
            let name = format!("Category {}", self.cfg.categories.len());
            self.cfg.categories.push(rdm_core::category::Category { dir: base.join(&name), name, extensions: Vec::new() });
        }
    }

    fn downloads(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.cfg.show_progress_window, "Show the progress window when a download starts");
        ui.checkbox(&mut self.cfg.show_complete_dialog, "Show the download complete dialog");
        ui.checkbox(&mut self.cfg.set_file_time, "Set the file date to the date reported by the server");
        ui.add_space(8.0);
        ui.label(RichText::new("If a file with the same name exists, a number is added to the new file's name.").weak());
    }

    fn connection(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Default number of connections");
            let text = match self.cfg.connections {
                Connections::Auto => "Auto".to_string(),
                Connections::Fixed(n) => n.to_string(),
            };
            egui::ComboBox::from_id_salt("conns").width(80.0).selected_text(text).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.cfg.connections, Connections::Auto, "Auto");
                for n in CONNECTION_CHOICES {
                    ui.selectable_value(&mut self.cfg.connections, Connections::Fixed(n), n.to_string());
                }
            });
        });
        ui.label(RichText::new("Auto starts with 8 and adds connections for as long as they increase the speed.").weak());
        ui.horizontal(|ui| {
            ui.label("Most connections Auto may use");
            ui.add(egui::DragValue::new(&mut self.cfg.max_connections).range(1..=32));
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.cfg.limiter_enabled, "Limit total download speed to");
            ui.add_enabled(self.cfg.limiter_enabled, egui::DragValue::new(&mut self.limit_kb).range(1..=10_000_000).suffix(" KB/s"));
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("Retries before giving up");
            ui.add(egui::DragValue::new(&mut self.cfg.max_retries).range(0..=1000));
            ui.add_space(12.0);
            ui.label("Connection timeout");
            ui.add(egui::DragValue::new(&mut self.cfg.stall_timeout_secs).range(5..=600).suffix(" s"));
        });
        ui.add_space(10.0);
        ui.label(RichText::new("Exceptions — sites that tolerate fewer connections").strong());
        let mut remove = None;
        for (i, l) in self.cfg.site_limits.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}: at most {} connection(s)", l.host, l.connections));
                if ui.small_button("×").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.cfg.site_limits.remove(i);
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.new_limit.host).desired_width(220.0).hint_text("example.com"));
            ui.add(egui::DragValue::new(&mut self.new_limit.connections).range(1..=32));
            if ui.add_enabled(!self.new_limit.host.trim().is_empty(), egui::Button::new("Add")).clicked() {
                self.cfg.site_limits.push(SiteLimit { host: self.new_limit.host.trim().to_ascii_lowercase(), connections: self.new_limit.connections });
                self.new_limit.host.clear();
            }
        });
    }

    fn proxy(&mut self, ui: &mut egui::Ui) {
        ui.radio_value(&mut self.proxy_mode, 0, "Use the system proxy settings");
        ui.radio_value(&mut self.proxy_mode, 1, "No proxy");
        ui.radio_value(&mut self.proxy_mode, 2, "Manual proxy");
        ui.add_enabled_ui(self.proxy_mode == 2, |ui| {
            egui::Grid::new("proxy").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Address");
                ui.add(egui::TextEdit::singleline(&mut self.proxy_url).desired_width(320.0).hint_text("http://host:8080  or  socks5://host:1080"));
                ui.end_row();
                ui.label("User name");
                ui.add(egui::TextEdit::singleline(&mut self.proxy_user).desired_width(200.0));
                ui.end_row();
                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut self.proxy_pass).password(true).desired_width(200.0));
                ui.end_row();
            });
        });
    }

    fn logins(&mut self, ui: &mut egui::Ui) {
        ui.label("Credentials sent automatically (HTTP Basic) to these sites:");
        let mut remove = None;
        for (i, l) in self.cfg.site_logins.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}  —  {}", l.host, l.user));
                if ui.small_button("×").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.cfg.site_logins.remove(i);
        }
        ui.add_space(6.0);
        egui::Grid::new("login").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Site");
            ui.add(egui::TextEdit::singleline(&mut self.new_login.host).desired_width(240.0).hint_text("example.com"));
            ui.end_row();
            ui.label("Login");
            ui.add(egui::TextEdit::singleline(&mut self.new_login.user).desired_width(200.0));
            ui.end_row();
            ui.label("Password");
            ui.add(egui::TextEdit::singleline(&mut self.new_login.password).password(true).desired_width(200.0));
            ui.end_row();
        });
        let ready = !self.new_login.host.trim().is_empty() && !self.new_login.user.is_empty();
        if ui.add_enabled(ready, egui::Button::new("Add login")).clicked() {
            self.new_login.host = self.new_login.host.trim().to_ascii_lowercase();
            self.cfg.site_logins.push(self.new_login.clone());
            self.new_login = SiteLogin { host: String::new(), user: String::new(), password: String::new() };
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Passwords are stored unencrypted in RDM's settings file.").color(Color32::from_rgb(0xB0, 0x70, 0x10)));
    }
}

// ---------------------------------------------------------------- Scheduler

pub struct SchedulerDialog {
    selected: Id,
    edit: Option<Queue>,
    new_name: String,
}

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

impl SchedulerDialog {
    pub fn new(selected: Id) -> Self {
        Self { selected, edit: None, new_name: String::new() }
    }

    pub fn show(&mut self, ctx: &egui::Context, manager: &Manager, views: &[View]) -> bool {
        let queues = manager.queues();
        if !queues.iter().any(|q| q.id == self.selected) {
            self.selected = MAIN_QUEUE;
            self.edit = None;
        }
        if self.edit.as_ref().map(|q| q.id) != Some(self.selected) {
            self.edit = queues.iter().find(|q| q.id == self.selected).cloned();
        }
        let running = queues.iter().find(|q| q.id == self.selected).is_some_and(|q| q.running);

        let mut open = true;
        let mut close = false;
        dialog("Scheduler").open(&mut open).show(ctx, |ui| {
            ui.set_width(660.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(180.0);
                    ui.label(RichText::new("Queues").strong());
                    for q in &queues {
                        let label = if q.running { format!("{} (running)", q.name) } else { q.name.clone() };
                        if ui.selectable_label(q.id == self.selected, label).clicked() {
                            self.selected = q.id;
                        }
                    }
                    ui.add_space(8.0);
                    ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(170.0).hint_text("New queue name"));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!self.new_name.trim().is_empty(), egui::Button::new("New queue")).clicked() {
                            self.selected = manager.add_queue(self.new_name.trim().to_string());
                            self.new_name.clear();
                        }
                        if ui.add_enabled(self.selected != MAIN_QUEUE, egui::Button::new("Delete")).clicked() {
                            manager.remove_queue(self.selected);
                        }
                    });
                });
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    let Some(q) = self.edit.as_mut() else { return };
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        ui.add_enabled(q.id != MAIN_QUEUE, egui::TextEdit::singleline(&mut q.name).desired_width(240.0));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Download");
                        ui.add(egui::DragValue::new(&mut q.max_concurrent).range(1..=10));
                        ui.label("files at the same time");
                    });
                    ui.add_space(6.0);
                    let s = &mut q.schedule;
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut s.start_enabled, "Start download at");
                        ui.add_enabled(s.start_enabled, egui::DragValue::new(&mut s.start_hour).range(0..=23).custom_formatter(|v, _| format!("{v:02}")));
                        ui.label(":");
                        ui.add_enabled(s.start_enabled, egui::DragValue::new(&mut s.start_minute).range(0..=59).custom_formatter(|v, _| format!("{v:02}")));
                    });
                    ui.add_enabled_ui(s.start_enabled, |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(22.0);
                            for (i, day) in DAYS.iter().enumerate() {
                                ui.toggle_value(&mut s.days[i], *day);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(22.0);
                            ui.label(RichText::new("No day selected means every day.").weak());
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut s.stop_enabled, "Stop download at");
                        ui.add_enabled(s.stop_enabled, egui::DragValue::new(&mut s.stop_hour).range(0..=23).custom_formatter(|v, _| format!("{v:02}")));
                        ui.label(":");
                        ui.add_enabled(s.stop_enabled, egui::DragValue::new(&mut s.stop_minute).range(0..=59).custom_formatter(|v, _| format!("{v:02}")));
                    });
                    ui.horizontal(|ui| {
                        ui.label("When the queue is done");
                        let name = |a: PostAction| match a {
                            PostAction::Nothing => "Do nothing",
                            PostAction::ExitApp => "Exit RDM",
                            PostAction::Sleep => "Put the computer to sleep",
                            PostAction::Hibernate => "Hibernate",
                            PostAction::Shutdown => "Shut down the computer",
                        };
                        egui::ComboBox::from_id_salt("post").width(200.0).selected_text(name(s.on_complete)).show_ui(ui, |ui| {
                            for a in [PostAction::Nothing, PostAction::ExitApp, PostAction::Sleep, PostAction::Hibernate, PostAction::Shutdown] {
                                ui.selectable_value(&mut s.on_complete, a, name(a));
                            }
                        });
                    });

                    ui.add_space(6.0);
                    let members: Vec<&View> = views.iter().filter(|v| v.d.queue == Some(q.id)).collect();
                    ui.label(RichText::new(format!("Files in the queue ({})", members.len())).strong());
                    egui::ScrollArea::vertical().max_height(120.0).min_scrolled_height(120.0).auto_shrink([false, false]).show(ui, |ui| {
                        if members.is_empty() {
                            ui.label(RichText::new("Empty. Use \"Download Later\" or right-click a download and choose Move to queue.").weak());
                        }
                        for v in members {
                            ui.horizontal(|ui| {
                                if ui.small_button("×").on_hover_text("Remove from queue").clicked() {
                                    manager.set_queue(v.d.id, None);
                                }
                                let status = match v.d.status {
                                    Status::Complete => "done",
                                    Status::Running => "running",
                                    Status::Failed => "error",
                                    _ => "waiting",
                                };
                                ui.add(egui::Label::new(format!("{}  ({status})", v.d.file_name)).truncate());
                            });
                        }
                    });
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                let apply = |q: &Option<Queue>| {
                    if let Some(q) = q {
                        manager.update_queue(q.clone());
                        manager.save_now();
                    }
                };
                if ui.add_enabled(!running, egui::Button::new("Start now").min_size([100.0, 24.0].into())).clicked() {
                    apply(&self.edit);
                    manager.start_queue(self.selected);
                }
                if ui.add_enabled(running, egui::Button::new("Stop").min_size([80.0, 24.0].into())).clicked() {
                    manager.stop_queue(self.selected);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_sized([80.0, 24.0], egui::Button::new("Close")).clicked() {
                        close = true;
                    }
                    if ui.add_sized([80.0, 24.0], egui::Button::new("Apply")).clicked() {
                        apply(&self.edit);
                    }
                });
            });
        });
        open && !close
    }
}

// ---------------------------------------------------------------- Power

/// "Shutting down in 60 s" with a way out.
pub struct PowerCountdown {
    pub action: PostAction,
    pub deadline: Instant,
}

impl PowerCountdown {
    /// `Some(true)`: time is up, do it. `Some(false)`: cancelled.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<bool> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Some(true);
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let what = match self.action {
            PostAction::Shutdown => "shut down",
            PostAction::Sleep => "go to sleep",
            PostAction::Hibernate => "hibernate",
            _ => return Some(false),
        };
        let mut result = None;
        egui::Modal::new(egui::Id::new("power")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(RichText::new("Queue finished").heading());
            ui.label(format!("The computer will {what} in {} seconds.", left.as_secs() + 1));
            ui.add_space(8.0);
            if ui.add_sized([120.0, 26.0], egui::Button::new("Cancel")).clicked() {
                result = Some(false);
            }
        });
        result
    }
}

pub fn run_power_action(action: PostAction) {
    #[cfg(windows)]
    {
        use std::process::Command;
        let _ = match action {
            PostAction::Shutdown => Command::new("shutdown").args(["/s", "/t", "0"]).spawn(),
            PostAction::Hibernate => Command::new("shutdown").args(["/h"]).spawn(),
            PostAction::Sleep => Command::new("rundll32.exe").args(["powrprof.dll,SetSuspendState", "0,1,0"]).spawn(),
            _ => return,
        };
    }
    #[cfg(not(windows))]
    let _ = action;
}

// ---------------------------------------------------------------- Link grabber

/// "Site grabber" for a single page: list the files it links to and pick some.
pub struct GrabDialog {
    url: String,
    extensions: String,
    ticket: Option<rdm_core::manager::GrabTicket>,
    links: Vec<(bool, rdm_core::grabber::GrabbedLink)>,
    error: Option<String>,
    searched: bool,
    start_now: bool,
}

impl GrabDialog {
    pub fn new(config: &Config) -> Self {
        Self {
            url: String::new(),
            extensions: config.capture_extensions.join(" "),
            ticket: None,
            links: Vec::new(),
            error: None,
            searched: false,
            start_now: false,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, manager: &Manager) -> bool {
        if let Some(result) = self.ticket.as_ref().and_then(|t| t.result()) {
            self.ticket = None;
            self.searched = true;
            match result {
                Ok(links) => {
                    self.links = links.into_iter().map(|l| (true, l)).collect();
                    self.error = None;
                }
                Err(e) => {
                    self.links.clear();
                    self.error = Some(e);
                }
            }
        }
        let busy = self.ticket.is_some();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let mut open = true;
        let mut close = false;
        dialog("Grab links from a page").open(&mut open).show(ctx, |ui| {
            ui.set_width(620.0);
            egui::Grid::new("grab").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Page address");
                ui.add(egui::TextEdit::singleline(&mut self.url).desired_width(500.0).hint_text("https://…"));
                ui.end_row();
                ui.label("File types");
                ui.add(egui::TextEdit::singleline(&mut self.extensions).desired_width(500.0).hint_text("leave empty for every linked file"));
                ui.end_row();
            });
            let url_ok = self.url.trim().starts_with("http://") || self.url.trim().starts_with("https://");
            ui.horizontal(|ui| {
                if ui.add_enabled(url_ok && !busy, egui::Button::new("Find links").min_size([110.0, 24.0].into())).clicked() {
                    let extensions = self.extensions.split_whitespace().map(|e| e.trim_start_matches('.').to_ascii_lowercase()).collect();
                    self.ticket = Some(manager.grab(RequestSpec::new(self.url.trim()), extensions));
                }
                if busy {
                    ui.spinner();
                    ui.label("Reading the page…");
                }
            });
            if let Some(err) = &self.error {
                ui.label(RichText::new(err).color(RED));
            }

            ui.separator();
            let chosen = self.links.iter().filter(|(on, _)| *on).count();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} files found, {chosen} selected", self.links.len())).strong());
                if ui.small_button("All").clicked() {
                    self.links.iter_mut().for_each(|(on, _)| *on = true);
                }
                if ui.small_button("None").clicked() {
                    self.links.iter_mut().for_each(|(on, _)| *on = false);
                }
            });
            egui::ScrollArea::vertical().max_height(220.0).min_scrolled_height(220.0).auto_shrink([false, false]).show(ui, |ui| {
                if self.links.is_empty() && self.searched && self.error.is_none() {
                    ui.label(RichText::new("No links to files of these types on that page.").weak());
                }
                for (i, (on, link)) in self.links.iter_mut().enumerate() {
                    ui.push_id(i, |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(on, "");
                            ui.add(egui::Label::new(link.file_name.as_str()).truncate()).on_hover_text(link.url.as_str());
                        });
                    });
                }
            });
            ui.separator();
            ui.checkbox(&mut self.start_now, "Start downloading immediately (otherwise add to the main queue)");
            ui.horizontal(|ui| {
                if ui.add_enabled(chosen > 0, egui::Button::new(format!("Add {chosen} downloads")).min_size([150.0, 24.0].into())).clicked() {
                    for (_, link) in self.links.iter().filter(|(on, _)| *on) {
                        let mut new = NewDownload::new(RequestSpec { url: link.url.clone(), referer: Some(self.url.trim().to_string()), ..Default::default() });
                        new.start = self.start_now;
                        new.queue = (!self.start_now).then_some(MAIN_QUEUE);
                        manager.add(new);
                    }
                    close = true;
                }
                if ui.add_sized([90.0, 24.0], egui::Button::new("Close")).clicked() {
                    close = true;
                }
            });
        });
        open && !close
    }
}
