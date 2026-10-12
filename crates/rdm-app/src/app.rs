//! The main window: menu, toolbar, category tree, download list, status bar.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Layout, RichText};
use egui_extras::{Column, TableBuilder};
use rdm_core::manager::{AddRequest, Event, PostAction, MAIN_QUEUE};
use rdm_core::util::{eta_secs, format_duration, format_size, format_speed};
use rdm_core::{Config, Id, Manager, NewDownload, Queue, RequestSpec, Status, View};

use crate::dialogs::{AddDialog, AddOutcome, BatchDialog, DeleteDialog, GrabDialog, OptionsDialog, PowerCountdown, PropertiesDialog, SchedulerDialog};
use crate::progress::{reveal_in_folder, CompleteDialog, ProgressWindow};
use crate::theme::{self, category_color, GREEN, ORANGE, RED};
use crate::tray::{Tray, TrayAction};
use crate::widgets::{file_badge, mini_progress, tool_button, Icon};

/// Launch options that belong to this process (not forwarded to a running instance).
#[derive(Default, Clone)]
pub struct Launch {
    pub data_dir: Option<PathBuf>,
    /// Render for a moment, save a PNG of the window here, and exit.
    pub screenshot: Option<PathBuf>,
    /// With `screenshot`: which dialog to have open (`add`, `options`, `scheduler`, `progress`).
    pub show: Option<String>,
    /// Start hidden in the notification area.
    pub minimized: bool,
    /// No tray icon, no clipboard access, child windows embedded (automated tests).
    pub headless: bool,
    /// IDM-style arguments (`/d URL` …) to act on.
    pub args: Vec<String>,
}

#[derive(Clone, PartialEq)]
enum Filter {
    All,
    Category(String),
    Unfinished,
    Finished,
    Queue(Id),
}

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Name,
    Size,
    Status,
    TimeLeft,
    Rate,
    LastTry,
    Description,
}

pub struct RdmApp {
    manager: Manager,
    config: Config,
    queues: Vec<Queue>,
    views: Vec<View>,
    refreshed: Instant,
    stale: Arc<AtomicBool>,

    filter: Filter,
    search: String,
    selected: BTreeSet<Id>,
    anchor: Option<Id>,
    sort: Option<(Col, bool)>,

    add: Option<AddDialog>,
    options: Option<OptionsDialog>,
    scheduler: Option<SchedulerDialog>,
    delete: Option<DeleteDialog>,
    properties: Option<PropertiesDialog>,
    batch: Option<BatchDialog>,
    grab: Option<GrabDialog>,
    about: bool,
    power: Option<PowerCountdown>,
    progress: BTreeMap<Id, ProgressWindow>,
    completed: Vec<CompleteDialog>,

    clipboard: Option<arboard::Clipboard>,
    clipboard_seen: String,
    clipboard_checked: Instant,

    tray: Option<Tray>,
    /// The window is hidden in the notification area.
    hidden: bool,
    /// Downloads started with `/q`: exit once they have all finished.
    quit_after: BTreeSet<Id>,
    /// Short message shown in the status bar for a few seconds.
    notice: Option<(String, Instant)>,

    api_status: String,
    launch: Launch,
    started: Instant,
    shot_requested: bool,
    quit: bool,
}

fn percent(v: &View) -> Option<f32> {
    v.d.size.filter(|s| *s > 0).map(|s| (v.d.downloaded as f32 * 100.0 / s as f32).min(100.0))
}

impl RdmApp {
    pub fn new(cc: &eframe::CreationContext<'_>, data_dir: PathBuf, launch: Launch) -> Result<Self, String> {
        let ctx = cc.egui_ctx.clone();
        // Set by the engine whenever something changed, so the list is re-read on the next frame.
        let stale = Arc::new(AtomicBool::new(true));
        let flag = stale.clone();
        let manager = Manager::new(data_dir, move || {
            flag.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        })
        .map_err(|e| e.to_string())?;
        let config = manager.config();
        theme::apply(&cc.egui_ctx, config.dark_mode);

        let api_status = match manager.start_api() {
            Ok(port) => format!("Listening for the browser extension on 127.0.0.1:{port}"),
            Err(e) => format!("Browser integration is off: port {} is unavailable ({e})", config.api_port),
        };
        if !launch.args.is_empty() {
            for event in rdm_core::api::parse_cli(&launch.args) {
                manager.push_event(event);
            }
        }
        if launch.screenshot.is_some() || launch.headless {
            // One image (or one test surface) must contain the child windows too.
            cc.egui_ctx.set_embed_viewports(true);
        }

        let tray = if launch.screenshot.is_some() || launch.headless { None } else { Tray::new(&cc.egui_ctx) };
        // Starting hidden only makes sense if there is an icon to bring the window back.
        let hidden = launch.minimized && tray.is_some();
        if launch.minimized && !hidden {
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }

        let mut app = Self {
            queues: manager.queues(),
            views: manager.views(),
            config,
            manager,
            refreshed: Instant::now(),
            stale,
            filter: Filter::All,
            search: String::new(),
            selected: BTreeSet::new(),
            anchor: None,
            sort: None,
            add: None,
            options: None,
            scheduler: None,
            delete: None,
            properties: None,
            batch: None,
            grab: None,
            about: false,
            power: None,
            progress: BTreeMap::new(),
            completed: Vec::new(),
            clipboard: if launch.headless { None } else { arboard::Clipboard::new().ok() },
            clipboard_seen: String::new(),
            clipboard_checked: Instant::now(),
            tray,
            hidden,
            quit_after: BTreeSet::new(),
            notice: None,
            api_status,
            launch,
            started: Instant::now(),
            shot_requested: false,
            quit: false,
        };
        // Whatever is on the clipboard at start-up is old news.
        app.clipboard_seen = app.clipboard.as_mut().and_then(|c| c.get_text().ok()).unwrap_or_default();
        Ok(app)
    }

    pub fn manager(&self) -> &Manager {
        &self.manager
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    /// Bring the window back from the notification area / taskbar.
    fn show_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        let actions = self.tray.as_ref().map(|t| t.poll()).unwrap_or_default();
        for action in actions {
            match action {
                TrayAction::Show => self.show_window(ctx),
                TrayAction::AddUrl => {
                    self.show_window(ctx);
                    self.open_add();
                }
                TrayAction::StartQueue => self.manager.start_queue(MAIN_QUEUE),
                TrayAction::StopAll => self.manager.pause_all(),
                TrayAction::Exit => self.quit = true,
            }
        }
    }

    /// The window's close button: keep running in the tray if configured.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.quit {
            return;
        }
        if self.tray.is_some() && self.config.close_to_tray {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
        }
    }

    /// Files dropped on the window (link lists, `.url` shortcuts) and links
    /// pasted with Ctrl+V while no text field has the focus.
    fn handle_drop_and_paste(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for path in dropped {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match self.manager.import_list(&path) {
                Ok(0) => self.notify(format!("No new download links found in {name}")),
                Ok(n) => self.notify(format!("Added {n} download(s) from {name}")),
                Err(e) => self.notify(format!("Cannot read {name}: {e}")),
            }
            self.refresh();
        }
        if self.add.is_some() || ctx.egui_wants_keyboard_input() {
            return;
        }
        let pasted = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Paste(text) => Some(text.trim().to_string()),
                _ => None,
            })
        });
        if let Some(url) = pasted.filter(|u| (u.starts_with("http://") || u.starts_with("https://")) && !u.contains(char::is_whitespace)) {
            self.add = Some(AddDialog::from_request(AddRequest { url, ..Default::default() }, &self.manager));
        }
    }

    fn import_dialog(&mut self) {
        let picked = rfd::FileDialog::new().add_filter("Link lists", &["json", "txt", "url"]).add_filter("All files", &["*"]).pick_file();
        if let Some(path) = picked {
            match self.manager.import_list(&path) {
                Ok(n) => self.notify(format!("Imported {n} download(s)")),
                Err(e) => self.notify(format!("Import failed: {e}")),
            }
            self.refresh();
        }
    }

    fn export_dialog(&mut self) {
        let picked = rfd::FileDialog::new()
            .add_filter("RDM list (JSON)", &["json"])
            .add_filter("Addresses only (text)", &["txt"])
            .set_file_name("rdm-downloads.json")
            .save_file();
        if let Some(path) = picked {
            match self.manager.export_list(&path) {
                Ok(n) => self.notify(format!("Exported {n} download(s) to {}", path.display())),
                Err(e) => self.notify(format!("Export failed: {e}")),
            }
        }
    }

    fn refresh(&mut self) {
        self.views = self.manager.views();
        self.queues = self.manager.queues();
        self.refreshed = Instant::now();
        let alive: BTreeSet<Id> = self.views.iter().map(|v| v.d.id).collect();
        self.selected.retain(|id| alive.contains(id));
    }

    fn view(&self, id: Id) -> Option<&View> {
        self.views.iter().find(|v| v.d.id == id)
    }

    fn open_progress(&mut self, id: Id) {
        let announce = self.config.show_complete_dialog;
        let view = self.manager.view(id);
        self.progress.entry(id).or_insert_with(|| ProgressWindow::new(id, view.as_ref(), announce));
    }

    // ---- Actions on the selection --------------------------------------

    fn selected_views(&self) -> Vec<&View> {
        self.views.iter().filter(|v| self.selected.contains(&v.d.id)).collect()
    }

    fn resume_selected(&mut self) {
        let ids: Vec<Id> = self.selected_views().iter().filter(|v| !matches!(v.d.status, Status::Running | Status::Complete)).map(|v| v.d.id).collect();
        let show = self.config.show_progress_window && ids.len() == 1;
        for id in ids {
            self.manager.start(id);
            if show {
                self.open_progress(id);
            }
        }
    }

    fn pause_selected(&mut self) {
        for v in self.selected_views() {
            self.manager.pause(v.d.id);
        }
    }

    fn ask_delete(&mut self, ids: Vec<Id>) {
        if ids.is_empty() {
            return;
        }
        let any_complete = self.views.iter().any(|v| ids.contains(&v.d.id) && v.d.status == Status::Complete);
        self.delete = Some(DeleteDialog { ids, any_complete, delete_files: false });
    }

    fn open_add(&mut self) {
        // Offer a link that is already on the clipboard, like IDM does.
        let clip = self.clipboard.as_mut().and_then(|c| c.get_text().ok()).unwrap_or_default();
        let clip = clip.trim();
        let url = if (clip.starts_with("http://") || clip.starts_with("https://")) && !clip.contains(char::is_whitespace) { clip.to_string() } else { String::new() };
        self.add = Some(AddDialog::new(url));
    }

    // ---- Events from the engine and from outside -----------------------

    fn handle_events(&mut self, ctx: &egui::Context) {
        let events = self.manager.take_events();
        if events.is_empty() {
            return;
        }
        for event in events {
            match event {
                Event::Finished { id, ok } => {
                    let announce = match self.progress.remove(&id) {
                        Some(w) => w.announce,
                        None => self.config.show_complete_dialog,
                    };
                    if ok && announce {
                        self.completed.push(CompleteDialog { id });
                    }
                    if let Some(v) = self.manager.view(id) {
                        match &v.d.error {
                            Some(e) if !ok => log::warn!("{}: {e}", v.d.file_name),
                            _ if ok => log::info!("finished {}", v.d.file_name),
                            _ => {}
                        }
                    }
                    // `/q`: leave once everything started that way is done.
                    if self.quit_after.remove(&id) && self.quit_after.is_empty() && ok {
                        self.quit = true;
                    }
                }
                Event::AddRequest(req) => self.handle_add_request(req, ctx),
                Event::StartQueue(id) => self.manager.start_queue(id),
                Event::QueueFinished { action, .. } => match action {
                    PostAction::Nothing => {}
                    PostAction::ExitApp => self.quit = true,
                    action => self.power = Some(PowerCountdown { action, deadline: Instant::now() + Duration::from_secs(60) }),
                },
                Event::ShowWindow => self.show_window(ctx),
                Event::Quit => self.quit = true,
            }
        }
        self.refresh();
    }

    fn handle_add_request(&mut self, req: AddRequest, ctx: &egui::Context) {
        if req.silent {
            let mut new = NewDownload::new(RequestSpec {
                url: req.url,
                referer: req.referer,
                cookies: req.cookies,
                user_agent: req.user_agent,
                ..Default::default()
            });
            new.file_name = req.file_name;
            new.dir = req.dir;
            new.start = !req.queue_only;
            new.queue = req.queue_only.then_some(MAIN_QUEUE);
            let id = self.manager.add(new);
            if req.quit_when_done && !req.queue_only {
                self.quit_after.insert(id);
            }
        } else {
            self.add = Some(AddDialog::from_request(req, &self.manager));
            self.show_window(ctx);
        }
    }

    fn watch_clipboard(&mut self, ctx: &egui::Context) {
        if !self.config.monitor_clipboard || self.clipboard_checked.elapsed() < Duration::from_millis(800) {
            return;
        }
        self.clipboard_checked = Instant::now();
        let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) else { return };
        if text == self.clipboard_seen {
            return;
        }
        self.clipboard_seen = text.clone();
        let Some(url) = rdm_core::util::download_link(&text, &self.config.capture_extensions) else { return };
        if self.add.is_none() {
            log::info!("download link on the clipboard: {url}");
            self.add = Some(AddDialog::from_request(AddRequest { url, ..Default::default() }, &self.manager));
            self.show_window(ctx);
        }
    }

    // ---- Layout --------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Tasks", |ui| {
                if ui.button("Add new download…").clicked() {
                    self.open_add();
                    ui.close();
                }
                if ui.button("Add batch download…").clicked() {
                    self.batch = Some(BatchDialog::new());
                    ui.close();
                }
                if ui.button("Grab links from a page…").clicked() {
                    self.grab = Some(GrabDialog::new(&self.config));
                    ui.close();
                }
                ui.separator();
                if ui.button("Import list…").clicked() {
                    ui.close();
                    self.import_dialog();
                }
                if ui.button("Export list…").clicked() {
                    ui.close();
                    self.export_dialog();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    self.quit = true;
                    ui.close();
                }
            });
            ui.menu_button("Downloads", |ui| {
                let has_sel = !self.selected.is_empty();
                if ui.add_enabled(has_sel, egui::Button::new("Resume")).clicked() {
                    self.resume_selected();
                    ui.close();
                }
                if ui.add_enabled(has_sel, egui::Button::new("Stop")).clicked() {
                    self.pause_selected();
                    ui.close();
                }
                if ui.button("Stop all").clicked() {
                    self.manager.pause_all();
                    ui.close();
                }
                ui.separator();
                if ui.add_enabled(has_sel, egui::Button::new("Remove")).clicked() {
                    self.ask_delete(self.selected.iter().copied().collect());
                    ui.close();
                }
                if ui.button("Delete all completed").clicked() {
                    self.ask_delete(self.views.iter().filter(|v| v.d.status == Status::Complete).map(|v| v.d.id).collect());
                    ui.close();
                }
                ui.separator();
                if ui.button("Scheduler…").clicked() {
                    self.scheduler = Some(SchedulerDialog::new(MAIN_QUEUE));
                    ui.close();
                }
                if ui.button("Start main queue").clicked() {
                    self.manager.start_queue(MAIN_QUEUE);
                    ui.close();
                }
                if ui.button("Stop main queue").clicked() {
                    self.manager.stop_queue(MAIN_QUEUE);
                    ui.close();
                }
                ui.separator();
                let mut limiter = self.config.limiter_enabled;
                if ui.checkbox(&mut limiter, "Speed limiter").clicked() {
                    self.config.limiter_enabled = limiter;
                    let _ = self.manager.set_config(self.config.clone());
                    ui.close();
                }
                if ui.button("Options…").clicked() {
                    self.options = Some(OptionsDialog::new(self.manager.config(), self.api_status.clone()));
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                let mut dark = self.config.dark_mode;
                if ui.checkbox(&mut dark, "Dark theme").clicked() {
                    self.config.dark_mode = dark;
                    let _ = self.manager.set_config(self.config.clone());
                    theme::apply(ui.ctx(), dark);
                    ui.close();
                }
                if ui.button("Clear sorting").clicked() {
                    self.sort = None;
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About").clicked() {
                    self.about = true;
                    ui.close();
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let sel = self.selected_views();
        let can_resume = sel.iter().any(|v| !matches!(v.d.status, Status::Running | Status::Complete));
        let can_stop = sel.iter().any(|v| v.d.status == Status::Running);
        let any_running = self.views.iter().any(|v| v.d.status == Status::Running);
        let any_complete = self.views.iter().any(|v| v.d.status == Status::Complete);
        let has_sel = !sel.is_empty();
        let main_running = self.queues.iter().any(|q| q.id == MAIN_QUEUE && q.running);

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if tool_button(ui, Icon::Add, "Add URL", true) {
                self.open_add();
            }
            if tool_button(ui, Icon::Resume, "Resume", can_resume) {
                self.resume_selected();
            }
            if tool_button(ui, Icon::Pause, "Stop", can_stop) {
                self.pause_selected();
            }
            if tool_button(ui, Icon::StopAll, "Stop All", any_running) {
                self.manager.pause_all();
            }
            ui.separator();
            if tool_button(ui, Icon::Delete, "Delete", has_sel) {
                self.ask_delete(self.selected.iter().copied().collect());
            }
            if tool_button(ui, Icon::DeleteDone, "Delete Completed", any_complete) {
                self.ask_delete(self.views.iter().filter(|v| v.d.status == Status::Complete).map(|v| v.d.id).collect());
            }
            ui.separator();
            if tool_button(ui, Icon::Options, "Options", true) {
                self.options = Some(OptionsDialog::new(self.manager.config(), self.api_status.clone()));
            }
            if tool_button(ui, Icon::Scheduler, "Scheduler", true) {
                self.scheduler = Some(SchedulerDialog::new(MAIN_QUEUE));
            }
            if tool_button(ui, Icon::StartQueue, "Start Queue", !main_running) {
                self.manager.start_queue(MAIN_QUEUE);
            }
            if tool_button(ui, Icon::StopQueue, "Stop Queue", main_running) {
                self.manager.stop_queue(MAIN_QUEUE);
            }
            if tool_button(ui, Icon::Batch, "Batch", true) {
                self.batch = Some(BatchDialog::new());
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                ui.add(egui::TextEdit::singleline(&mut self.search).desired_width(170.0).hint_text("Search downloads"));
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let count = |f: &Filter, views: &[View]| views.iter().filter(|v| matches_filter(f, v)).count();
        let entry = |ui: &mut egui::Ui, this: &mut Self, indent: f32, filter: Filter, label: &str, color: Option<egui::Color32>| {
            ui.horizontal(|ui| {
                ui.add_space(indent);
                if let Some(c) = color {
                    file_badge(ui, c);
                }
                let n = count(&filter, &this.views);
                let text = if n > 0 { format!("{label}  ({n})") } else { label.to_string() };
                if ui.selectable_label(this.filter == filter, text).clicked() {
                    this.filter = filter;
                }
            });
        };
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("Categories").strong());
            entry(ui, self, 2.0, Filter::All, "All Downloads", None);
            let cats: Vec<String> = self.config.categories.iter().map(|c| c.name.clone()).collect();
            for name in cats {
                let color = category_color(&name);
                entry(ui, self, 14.0, Filter::Category(name.clone()), &name, Some(color));
            }
            ui.add_space(6.0);
            entry(ui, self, 2.0, Filter::Unfinished, "Unfinished", None);
            entry(ui, self, 2.0, Filter::Finished, "Finished", None);
            ui.add_space(6.0);
            ui.label(RichText::new("Queues").strong());
            let queues: Vec<(Id, String, bool)> = self.queues.iter().map(|q| (q.id, q.name.clone(), q.running)).collect();
            for (id, name, running) in queues {
                let label = if running { format!("{name} (running)") } else { name };
                entry(ui, self, 14.0, Filter::Queue(id), &label, None);
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let active = self.views.iter().filter(|v| v.d.status == Status::Running).count();
        let speed: u64 = self.views.iter().map(|v| v.speed).sum();
        ui.horizontal(|ui| {
            ui.label(format!("{} downloads", self.views.len()));
            ui.separator();
            ui.label(format!("{active} active"));
            ui.separator();
            ui.label(format!("Speed: {}", format_speed(speed)));
            ui.separator();
            if self.config.limiter_enabled {
                ui.label(RichText::new(format!("Limiter: {}", format_speed(self.config.speed_limit))).color(ORANGE));
            } else {
                ui.label("Limiter off");
            }
            if !self.selected.is_empty() {
                ui.separator();
                ui.label(format!("{} selected", self.selected.len()));
            }
            if let Some((text, since)) = &self.notice {
                if since.elapsed() < Duration::from_secs(6) {
                    ui.separator();
                    ui.label(RichText::new(text).strong());
                } else {
                    self.notice = None;
                }
            }
        });
    }

    fn visible_rows(&self) -> Vec<usize> {
        let needle = self.search.trim().to_lowercase();
        let mut rows: Vec<usize> = (0..self.views.len())
            .filter(|&i| {
                let v = &self.views[i];
                matches_filter(&self.filter, v)
                    && (needle.is_empty() || v.d.file_name.to_lowercase().contains(&needle) || v.d.spec.url.to_lowercase().contains(&needle))
            })
            .collect();
        match self.sort {
            Some((col, ascending)) => {
                let key = |i: &usize| {
                    let v = &self.views[*i];
                    match col {
                        Col::Name => SortKey::Text(v.d.file_name.to_lowercase()),
                        Col::Size => SortKey::Num(v.d.size.unwrap_or(0)),
                        Col::Status => SortKey::Num(percent(v).map(|p| (p * 100.0) as u64).unwrap_or(0) + if v.d.status == Status::Complete { 1 } else { 0 }),
                        Col::TimeLeft => SortKey::Num(eta_secs(v.d.size, v.d.downloaded, v.speed).unwrap_or(u64::MAX)),
                        Col::Rate => SortKey::Num(v.speed),
                        Col::LastTry => SortKey::Num(v.d.last_try_at.map(|t| t.timestamp() as u64).unwrap_or(0)),
                        Col::Description => SortKey::Text(v.d.description.to_lowercase()),
                    }
                };
                rows.sort_by(|a, b| if ascending { key(a).cmp(&key(b)) } else { key(b).cmp(&key(a)) });
            }
            // Newest first, like IDM's default view.
            None => rows.reverse(),
        }
        rows
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let rows = self.visible_rows();
        let modifiers = ui.input(|i| i.modifiers);
        let mut clicked: Option<(Id, bool)> = None; // (id, is_secondary)
        let mut activated: Option<Id> = None;
        let mut action: Option<RowAction> = None;
        let mut sort_click: Option<Col> = None;

        if self.views.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("No downloads yet.\nClick \"Add URL\" or copy a link and press it.").weak());
            });
            return;
        }

        let sort = self.sort;
        let header_cell = |ui: &mut egui::Ui, col: Col, title: &str, sort_click: &mut Option<Col>| {
            if ui.add(egui::Button::new(RichText::new(title).strong()).frame(false)).clicked() {
                *sort_click = Some(col);
            }
            if let Some((c, ascending)) = sort {
                if c == col {
                    crate::widgets::sort_arrow(ui, ascending);
                }
            }
        };

        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(270.0).at_least(120.0).clip(true))
            .column(Column::initial(22.0).at_least(20.0))
            .column(Column::initial(72.0).at_least(50.0).clip(true))
            .column(Column::initial(118.0).at_least(70.0).clip(true))
            .column(Column::initial(92.0).at_least(50.0).clip(true))
            .column(Column::initial(88.0).at_least(50.0).clip(true))
            .column(Column::initial(98.0).at_least(60.0).clip(true))
            .column(Column::remainder().at_least(80.0).clip(true))
            .header(22.0, |mut header| {
                header.col(|ui| header_cell(ui, Col::Name, "File Name", &mut sort_click));
                header.col(|ui| {
                    ui.label(RichText::new("Q").strong()).on_hover_text("In a queue");
                });
                header.col(|ui| header_cell(ui, Col::Size, "Size", &mut sort_click));
                header.col(|ui| header_cell(ui, Col::Status, "Status", &mut sort_click));
                header.col(|ui| header_cell(ui, Col::TimeLeft, "Time left", &mut sort_click));
                header.col(|ui| header_cell(ui, Col::Rate, "Transfer rate", &mut sort_click));
                header.col(|ui| header_cell(ui, Col::LastTry, "Last Try Date", &mut sort_click));
                header.col(|ui| header_cell(ui, Col::Description, "Description", &mut sort_click));
            })
            .body(|body| {
                body.rows(22.0, rows.len(), |mut row| {
                    let v = &self.views[rows[row.index()]];
                    let id = v.d.id;
                    row.set_selected(self.selected.contains(&id));

                    row.col(|ui| {
                        file_badge(ui, category_color(&v.d.category));
                        ui.label(&v.d.file_name);
                    });
                    row.col(|ui| {
                        if v.d.queue.is_some() {
                            crate::widgets::queue_dot(ui).on_hover_text(
                                self.queues.iter().find(|q| Some(q.id) == v.d.queue).map(|q| q.name.clone()).unwrap_or_default(),
                            );
                        }
                    });
                    row.col(|ui| {
                        ui.label(v.d.size.map(format_size).unwrap_or_default());
                    });
                    row.col(|ui| match v.d.status {
                        Status::Complete => {
                            ui.label(RichText::new("Complete").color(GREEN));
                        }
                        Status::Failed => {
                            ui.label(RichText::new("Error").color(RED)).on_hover_text(v.d.error.clone().unwrap_or_default());
                        }
                        Status::Queued if v.d.downloaded == 0 => {
                            ui.label("Waiting");
                        }
                        status => {
                            let p = percent(v);
                            let text = match (status, p) {
                                (Status::Running, Some(p)) => format!("{p:.2}%"),
                                (Status::Running, None) => format_size(v.d.downloaded),
                                (_, Some(p)) => format!("Paused {p:.1}%"),
                                (_, None) => "Paused".to_string(),
                            };
                            let color = if status == Status::Running { theme::ACCENT.gamma_multiply(0.55) } else { ORANGE.gamma_multiply(0.45) };
                            let resp = ui.scope(|ui| mini_progress(ui, p.unwrap_or(0.0) / 100.0, color, &text)).response;
                            if let Some(note) = &v.note {
                                resp.on_hover_text(note);
                            }
                        }
                    });
                    row.col(|ui| {
                        if v.d.status == Status::Running {
                            if let Some(s) = eta_secs(v.d.size, v.d.downloaded, v.speed) {
                                ui.label(format_duration(s));
                            }
                        }
                    });
                    row.col(|ui| {
                        if v.d.status == Status::Running && v.speed > 0 {
                            ui.label(format_speed(v.speed));
                        }
                    });
                    row.col(|ui| {
                        if let Some(t) = v.d.last_try_at {
                            ui.label(t.format("%b %d %H:%M").to_string());
                        }
                    });
                    row.col(|ui| {
                        ui.label(&v.d.description);
                    });

                    let resp = row.response();
                    if resp.double_clicked() {
                        activated = Some(id);
                    } else if resp.clicked() {
                        clicked = Some((id, false));
                    } else if resp.secondary_clicked() {
                        clicked = Some((id, true));
                    }
                    resp.context_menu(|ui| {
                        let done = v.d.status == Status::Complete;
                        let running = v.d.status == Status::Running;
                        let entries = [
                            (done, "Open", RowAction::Open(id), false),
                            (true, "Open folder", RowAction::OpenFolder(id), true),
                            (!done && !running, "Resume", RowAction::Resume, false),
                            (running, "Stop", RowAction::Stop, false),
                            (!running, "Redownload", RowAction::Redownload(id), false),
                            (true, "Remove", RowAction::Remove, true),
                        ];
                        for (enabled, text, a, separator_after) in entries {
                            if menu_item(ui, enabled, text) {
                                action = Some(a);
                            }
                            if separator_after {
                                ui.separator();
                            }
                        }
                        ui.menu_button("Move to queue", |ui| {
                            for q in &self.queues {
                                if menu_item(ui, true, q.name.as_str()) {
                                    action = Some(RowAction::SetQueue(Some(q.id)));
                                }
                            }
                        });
                        if menu_item(ui, v.d.queue.is_some(), "Delete from queue") {
                            action = Some(RowAction::SetQueue(None));
                        }
                        ui.separator();
                        for (text, a) in [
                            ("Copy address", RowAction::CopyUrl(id)),
                            ("Show progress", RowAction::Progress(id)),
                            ("Properties", RowAction::Properties(id)),
                        ] {
                            if menu_item(ui, true, text) {
                                action = Some(a);
                            }
                        }
                    });
                });
            });

        if let Some(col) = sort_click {
            self.sort = match self.sort {
                Some((c, true)) if c == col => Some((col, false)),
                Some((c, false)) if c == col => None,
                _ => Some((col, true)),
            };
        }
        if let Some((id, secondary)) = clicked {
            self.click_row(id, secondary, modifiers, &rows);
        }
        if let Some(id) = activated {
            self.selected = BTreeSet::from([id]);
            match self.view(id).map(|v| v.d.status) {
                Some(Status::Complete) => self.run_row_action(RowAction::Open(id), ui.ctx()),
                Some(Status::Running) => self.open_progress(id),
                Some(_) => self.run_row_action(RowAction::Properties(id), ui.ctx()),
                None => {}
            }
        }
        if let Some(a) = action {
            self.run_row_action(a, ui.ctx());
        }
    }

    fn click_row(&mut self, id: Id, secondary: bool, modifiers: egui::Modifiers, rows: &[usize]) {
        if secondary {
            // Right-click keeps a multi-selection that contains the row.
            if !self.selected.contains(&id) {
                self.selected = BTreeSet::from([id]);
                self.anchor = Some(id);
            }
        } else if modifiers.shift && self.anchor.is_some() {
            let ids: Vec<Id> = rows.iter().map(|&i| self.views[i].d.id).collect();
            let a = ids.iter().position(|x| Some(*x) == self.anchor);
            let b = ids.iter().position(|x| *x == id);
            if let (Some(a), Some(b)) = (a, b) {
                self.selected = ids[a.min(b)..=a.max(b)].iter().copied().collect();
            }
        } else if modifiers.command {
            if !self.selected.remove(&id) {
                self.selected.insert(id);
            }
            self.anchor = Some(id);
        } else {
            self.selected = BTreeSet::from([id]);
            self.anchor = Some(id);
        }
    }

    fn run_row_action(&mut self, action: RowAction, ctx: &egui::Context) {
        match action {
            RowAction::Open(id) => {
                if let Some(v) = self.view(id) {
                    let _ = open::that_detached(v.d.path());
                }
            }
            RowAction::OpenFolder(id) => {
                if let Some(v) = self.view(id) {
                    reveal_in_folder(&v.d.path());
                }
            }
            RowAction::Resume => self.resume_selected(),
            RowAction::Stop => self.pause_selected(),
            RowAction::Redownload(id) => {
                self.manager.restart(id);
                if self.config.show_progress_window {
                    self.open_progress(id);
                }
            }
            RowAction::Remove => self.ask_delete(self.selected.iter().copied().collect()),
            RowAction::SetQueue(queue) => {
                for id in self.selected.clone() {
                    self.manager.set_queue(id, queue);
                }
            }
            RowAction::CopyUrl(id) => {
                if let Some(v) = self.view(id) {
                    ctx.copy_text(v.d.spec.url.clone());
                    // Our own copy must not trigger the clipboard monitor.
                    self.clipboard_seen = v.d.spec.url.clone();
                }
            }
            RowAction::Progress(id) => self.open_progress(id),
            RowAction::Properties(id) => {
                if let Some(v) = self.view(id) {
                    self.properties = Some(PropertiesDialog::new(v));
                }
            }
        }
        self.refresh();
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(mut d) = self.add.take() {
            match d.show(ctx, &self.manager, &self.config) {
                AddOutcome::Keep => self.add = Some(d),
                AddOutcome::Close => {}
                AddOutcome::Added { id, started, quit_when_done } => {
                    self.refresh();
                    self.selected = BTreeSet::from([id]);
                    if started && self.config.show_progress_window {
                        self.open_progress(id);
                    }
                    if started && quit_when_done {
                        self.quit_after.insert(id);
                    }
                }
            }
        }
        if let Some(mut d) = self.grab.take() {
            if d.show(ctx, &self.manager) {
                self.grab = Some(d);
            } else {
                self.refresh();
            }
        }
        if let Some(mut d) = self.delete.take() {
            match d.show(ctx) {
                None => self.delete = Some(d),
                Some(false) => {}
                Some(true) => {
                    for id in &d.ids {
                        self.progress.remove(id);
                        self.manager.remove(*id, d.delete_files);
                    }
                    self.refresh();
                }
            }
        }
        if let Some(mut d) = self.options.take() {
            let mut applied = false;
            if d.show(ctx, &self.manager, &mut applied) {
                self.options = Some(d);
            }
            if applied {
                self.config = self.manager.config();
                theme::apply(ctx, self.config.dark_mode);
            }
        }
        if let Some(mut d) = self.scheduler.take() {
            if d.show(ctx, &self.manager, &self.views) {
                self.scheduler = Some(d);
            }
        }
        if let Some(mut d) = self.properties.take() {
            if d.show(ctx, &self.manager) {
                self.properties = Some(d);
            }
        }
        if let Some(mut d) = self.batch.take() {
            if d.show(ctx, &self.manager) {
                self.batch = Some(d);
            } else {
                self.refresh();
            }
        }
        if let Some(mut p) = self.power.take() {
            match p.show(ctx) {
                None => self.power = Some(p),
                Some(false) => {}
                Some(true) => {
                    self.manager.shutdown();
                    crate::dialogs::run_power_action(p.action);
                    self.quit = true;
                }
            }
        }
        if self.about {
            let mut open = true;
            egui::Window::new("About").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).open(&mut open).show(ctx, |ui| {
                ui.label(RichText::new("Rapid Download Manager").heading());
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.add_space(6.0);
                ui.label("A download accelerator written in Rust.");
                ui.label("Dynamic segmentation · up to 32 connections · resume · queues · scheduler");
            });
            self.about = open;
        }

        // Child windows.
        let manager = self.manager.clone();
        self.progress.retain(|_, w| w.show(ctx, &manager));
        let mut never_again = false;
        self.completed.retain_mut(|d| d.show(ctx, &manager, &mut never_again));
        if never_again {
            self.config.show_complete_dialog = false;
            let _ = self.manager.set_config(self.config.clone());
            self.completed.clear();
        }
    }

    /// `--screenshot`: open the requested dialog, wait for things to settle, capture, exit.
    fn screenshot_mode(&mut self, ctx: &egui::Context) {
        let Some(path) = self.launch.screenshot.clone() else { return };
        ctx.request_repaint();
        let age = self.started.elapsed();
        if let Some(what) = self.launch.show.take() {
            match what.as_str() {
                "add" => self.add = Some(AddDialog::new("https://example.com/files/ubuntu-24.04-desktop-amd64.iso".into())),
                "options" => self.options = Some(OptionsDialog::new(self.manager.config(), self.api_status.clone())),
                "scheduler" => self.scheduler = Some(SchedulerDialog::new(MAIN_QUEUE)),
                "progress" => {
                    // Wait until something is actually running.
                    match self.views.iter().find(|v| v.d.status == Status::Running).map(|v| v.d.id) {
                        Some(id) => self.open_progress(id),
                        None if age < Duration::from_secs(8) => self.launch.show = Some(what),
                        None => {}
                    }
                }
                _ => {}
            }
        }
        if age > Duration::from_millis(4500) && !self.shot_requested {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let [w, h] = image.size;
            let rgba: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
            if let Err(e) = image::save_buffer(&path, &rgba, w as u32, h as u32, image::ColorType::Rgba8) {
                eprintln!("cannot save screenshot: {e}");
            }
            self.quit = true;
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortKey {
    Num(u64),
    Text(String),
}

enum RowAction {
    Open(Id),
    OpenFolder(Id),
    Resume,
    Stop,
    Redownload(Id),
    Remove,
    SetQueue(Option<Id>),
    CopyUrl(Id),
    Progress(Id),
    Properties(Id),
}

fn matches_filter(filter: &Filter, v: &View) -> bool {
    match filter {
        Filter::All => true,
        Filter::Category(c) => &v.d.category == c,
        Filter::Unfinished => v.d.status != Status::Complete,
        Filter::Finished => v.d.status == Status::Complete,
        Filter::Queue(q) => v.d.queue == Some(*q),
    }
}

/// A context-menu entry; closes the menu when chosen.
fn menu_item(ui: &mut egui::Ui, enabled: bool, text: &str) -> bool {
    let clicked = ui.add_enabled(enabled, egui::Button::new(text)).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

impl eframe::App for RdmApp {
    /// Runs every frame, and also while the window is hidden in the tray —
    /// so everything that must work without a visible window lives here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray(ctx);
        self.handle_events(ctx);
        self.watch_clipboard(ctx);
        self.handle_close_request(ctx);

        let active = self.views.iter().any(|v| v.d.status == Status::Running);
        if self.stale.swap(false, Ordering::Relaxed) || self.refreshed.elapsed() >= Duration::from_millis(if active { 100 } else { 500 }) {
            self.refresh();
        }
        if self.quit {
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
        }
        // Idle: wake only often enough to notice queue/schedule changes and the clipboard.
        let idle = if self.hidden { 1500 } else { 1000 };
        ctx.request_repaint_after(Duration::from_millis(if active && !self.hidden { 100 } else { idle }));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("categories").resizable(true).default_size(185.0).min_size(120.0).show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.table(ui));

        self.handle_drop_and_paste(&ctx);
        self.dialogs(&ctx);
        self.screenshot_mode(&ctx);

        // Delete key removes the selection (unless a text field has focus).
        if !self.selected.is_empty() && self.delete.is_none() && !ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            self.ask_delete(self.selected.iter().copied().collect());
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Pause everything and flush progress so the next start resumes cleanly.
        self.manager.shutdown();
    }
}
