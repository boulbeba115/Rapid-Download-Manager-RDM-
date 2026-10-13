//! The interface driven the way a user drives it: real clicks, typing and
//! key presses against the real application, with a real (local) server.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use rdm_app::app::{Launch, RdmApp};
use rdm_core::http::{ProxyConfig, RequestSpec};
use rdm_core::manager::MAIN_QUEUE;
use rdm_core::task::Connections;
use rdm_core::testserver::{verify_file, ServerOptions, TestServer};
use rdm_core::{Config, NewDownload, Status};

const MIB: u64 = 1024 * 1024;
static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Gui {
    h: Harness<'static, RdmApp>,
    root: PathBuf,
}

impl Gui {
    fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join("rdm-tests").join(format!("gui-{name}-{}-{n}", std::process::id()));
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let cfg = Config {
            categories: rdm_core::category::defaults(&root.join("downloads")),
            api_port: 0,
            net: rdm_core::NetConfig { proxy: ProxyConfig::None, ..Default::default() },
            ..Default::default()
        };
        rdm_core::state::save_json(&data.join("config.json"), &cfg).unwrap();

        let launch = Launch { headless: true, ..Default::default() };
        let h = Harness::builder()
            .with_size(egui::vec2(1120.0, 640.0))
            .build_eframe(move |cc| RdmApp::new(cc, data, launch).expect("app starts"));
        let mut gui = Self { h, root };
        gui.steps(3);
        gui
    }

    fn app(&self) -> &RdmApp {
        self.h.state()
    }

    fn server(&self, opts: ServerOptions) -> TestServer {
        self.app().manager().runtime().block_on(TestServer::start(opts))
    }

    fn steps(&mut self, n: usize) {
        for _ in 0..n {
            self.h.step();
        }
    }

    /// Keep the UI running until `done` holds, like a user waiting and watching.
    fn until(&mut self, what: &str, secs: u64, mut done: impl FnMut(&Harness<'static, RdmApp>) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.h.step();
            if done(&self.h) {
                // Let the frame that satisfied the condition settle.
                self.h.step();
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn click(&mut self, label: &str) {
        // Dialogs are drawn after the main window, so the last match is the one in front.
        self.h.get_all_by_label(label).last().unwrap_or_else(|| panic!("nothing on screen is labelled {label:?}")).click();
        self.steps(2);
    }

    fn has(&self, label: &str) -> bool {
        present(&self.h, label)
    }

    fn status_of(&self, file: &str) -> Option<Status> {
        self.app().manager().views().into_iter().find(|v| v.d.file_name == file).map(|v| v.d.status)
    }

    /// Type into the text field of the dialog that is open (the last text input on screen).
    fn type_in_dialog(&mut self, text: &str) {
        let field = self.h.get_all_by_role(Role::TextInput).last().expect("a text field is on screen");
        field.focus();
        self.steps(2);
        self.h.get_all_by_role(Role::TextInput).last().unwrap().type_text(text);
        self.steps(2);
    }

    fn queue(&self, url: &str, name: &str) -> rdm_core::Id {
        let mut new = NewDownload::new(RequestSpec::new(url));
        new.file_name = Some(name.into());
        new.start = false;
        self.app().manager().add(new)
    }
}

impl Drop for Gui {
    fn drop(&mut self) {
        self.app().manager().shutdown();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn add_url_download_and_delete_by_clicking() {
    let mut gui = Gui::new("add");
    let opts = ServerOptions::new(3 * MIB + 9);
    let (seed, size) = (opts.seed, opts.size);
    let server = gui.server(opts);

    // Toolbar → "Enter new address" dialog.
    assert!(gui.has("No downloads yet.\nClick \"Add URL\" or copy a link and press it."));
    gui.click("Add URL");
    gui.until("the address dialog", 5, |h| present(h, "Enter new address to download"));
    gui.type_in_dialog(&server.url());
    gui.click("OK");

    // The server is asked about the file; the Download File Info dialog follows.
    gui.until("Download File Info", 10, |h| present(h, "Start Download"));
    assert!(gui.has("Resumable"));
    gui.click("Start Download");

    gui.until("the download to finish", 30, |h| h.state().manager().views().first().is_some_and(|v| v.d.status == Status::Complete));
    let view = gui.app().manager().views().remove(0);
    assert_eq!(view.d.file_name, "file.bin");
    verify_file(&view.d.path(), seed, size).unwrap();

    // The download-complete dialog appears on its own; dismiss it.
    gui.until("the completion dialog", 5, |h| present(h, "Download complete"));
    gui.click("Close");
    gui.until("the completion dialog to go away", 5, |h| !present(h, "Download complete"));

    // Select the row, press Delete on the toolbar, confirm.
    gui.click("file.bin");
    gui.click("Delete");
    gui.until("the confirmation", 5, |h| present(h, "Confirm removal"));
    gui.click("Remove");
    gui.until("the list to empty", 5, |h| h.state().manager().views().is_empty());
    assert!(view.d.path().exists(), "the finished file stays unless the box is ticked");
}

#[test]
fn stop_and_resume_from_the_toolbar() {
    let mut gui = Gui::new("pause");
    let mut opts = ServerOptions::new(12 * MIB);
    opts.per_conn_rate = 512 * 1024;
    let (seed, size) = (opts.seed, opts.size);
    let server = gui.server(opts.clone());

    let mut new = NewDownload::new(RequestSpec::new(server.url()));
    new.file_name = Some("big.iso".into());
    new.connections = Some(Connections::Fixed(8));
    let id = gui.app().manager().add(new);
    gui.until("some progress", 20, |h| h.state().manager().view(id).is_some_and(|v| v.d.downloaded > 2 * MIB));

    gui.click("big.iso");
    gui.click("Stop");
    gui.until("the download to pause", 10, |h| h.state().manager().view(id).unwrap().d.status == Status::Paused);
    let kept = gui.app().manager().view(id).unwrap().d.downloaded;
    assert!(kept > 2 * MIB);

    // Lift the throttle so the rest is quick, then resume from the toolbar.
    server.update(|o| o.per_conn_rate = 0);
    gui.click("Resume");
    gui.until("the download to finish", 30, |h| h.state().manager().view(id).unwrap().d.status == Status::Complete);
    verify_file(&gui.app().manager().view(id).unwrap().d.path(), seed, size).unwrap();
}

#[test]
fn search_sort_and_context_menu() {
    let mut gui = Gui::new("list");
    gui.queue("http://127.0.0.1:9/a/alpha-report.pdf", "alpha-report.pdf");
    gui.queue("http://127.0.0.1:9/a/zeta-movie.mp4", "zeta-movie.mp4");
    gui.queue("http://127.0.0.1:9/a/middle-archive.zip", "middle-archive.zip");
    gui.until("the rows", 5, |h| present(h, "zeta-movie.mp4"));

    // Default order is newest first; sorting by name puts "alpha" on top.
    let y = |gui: &Gui, name: &str| gui.h.get_by_label(name).rect().center().y;
    assert!(y(&gui, "middle-archive.zip") < y(&gui, "alpha-report.pdf"));
    gui.click("File Name");
    assert!(y(&gui, "alpha-report.pdf") < y(&gui, "middle-archive.zip"));
    assert!(y(&gui, "middle-archive.zip") < y(&gui, "zeta-movie.mp4"));
    gui.click("File Name");
    assert!(y(&gui, "zeta-movie.mp4") < y(&gui, "alpha-report.pdf"), "second click reverses the order");

    // The category tree filters the list.
    gui.click("Video  (1)");
    assert!(gui.has("zeta-movie.mp4") && !gui.has("alpha-report.pdf"));
    gui.click("All Downloads  (3)");
    assert!(gui.has("alpha-report.pdf"));

    // Search box (the first text field on screen).
    let search = gui.h.get_all_by_role(Role::TextInput).next().unwrap();
    search.focus();
    gui.steps(2);
    gui.h.get_all_by_role(Role::TextInput).next().unwrap().type_text("archive");
    gui.steps(3);
    assert!(gui.has("middle-archive.zip"));
    assert!(!gui.has("alpha-report.pdf") && !gui.has("zeta-movie.mp4"));

    // Right-click → Properties opens the properties dialog for that row.
    gui.h.get_by_label("middle-archive.zip").click_secondary();
    gui.steps(3);
    gui.click("Properties");
    gui.until("the properties dialog", 5, |h| present(h, "Download properties"));
    gui.click("Cancel");
    gui.until("the dialog to close", 5, |h| !present(h, "Download properties"));

    // Right-click → Remove asks first, then removes.
    gui.h.get_by_label("middle-archive.zip").click_secondary();
    gui.steps(3);
    gui.click("Remove");
    gui.until("the confirmation", 5, |h| present(h, "Confirm removal"));
    gui.click("Remove");
    gui.until("two downloads left", 5, |h| h.state().manager().views().len() == 2);
}

#[test]
fn queue_runs_from_the_scheduler_dialog() {
    let mut gui = Gui::new("queue");
    let server = gui.server(ServerOptions::new(MIB));
    for name in ["q1.bin", "q2.bin"] {
        let mut new = NewDownload::new(RequestSpec::new(server.url()));
        new.file_name = Some(name.into());
        new.queue = Some(MAIN_QUEUE);
        new.start = false;
        gui.app().manager().add(new);
    }
    gui.steps(3);
    assert_eq!(gui.status_of("q1.bin"), Some(Status::Queued));

    gui.click("Scheduler");
    gui.until("the scheduler dialog", 5, |h| present(h, "Start now"));
    assert!(gui.has("Files in the queue (2)"));
    gui.click("Start now");
    gui.until("the queue to finish", 30, |h| h.state().manager().views().iter().all(|v| v.d.status == Status::Complete));
}

#[test]
fn options_are_applied_and_saved() {
    let mut gui = Gui::new("options");
    assert!(gui.app().manager().config().show_complete_dialog);

    gui.click("Options");
    gui.until("the options dialog", 5, |h| present(h, "Site logins"));
    gui.click("Downloads");
    gui.until("the Downloads tab", 5, |h| present(h, "Show the download complete dialog"));
    gui.click("Show the download complete dialog");
    gui.click("OK");
    gui.until("the dialog to close", 5, |h| !present(h, "Site logins"));

    assert!(!gui.app().manager().config().show_complete_dialog);
    // …and it is on disk, not just in memory.
    let saved: Config = rdm_core::state::load_json(&gui.root.join("data").join("config.json")).unwrap();
    assert!(!saved.show_complete_dialog);
}

#[test]
fn batch_dialog_adds_a_numbered_series() {
    let mut gui = Gui::new("batch");
    gui.click("Batch");
    gui.until("the batch dialog", 5, |h| present(h, "Batch download"));
    // The template field is pre-filled with an example covering parts 01–10.
    assert!(gui.has("First: https://example.com/files/part01.zip"));
    assert!(gui.has("Last:  https://example.com/files/part10.zip"));
    gui.click("Add 10 downloads");
    gui.until("ten queued downloads", 5, |h| h.state().manager().views().len() == 10);
    let views = gui.app().manager().views();
    assert!(views.iter().all(|v| v.d.status == Status::Queued && v.d.queue == Some(MAIN_QUEUE) && v.d.category == "Compressed"));
    assert!(views.iter().any(|v| v.d.file_name == "part07.zip"));
}

/// Whether anything on screen carries this label (a window title and a
/// heading may both do, so more than one match is fine).
fn present(h: &Harness<'static, RdmApp>, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}

/// The search box narrows by every word typed, looks beyond the file name,
/// says how many rows match, and explains an empty result.
#[test]
fn search_matches_every_word_and_can_be_cleared() {
    let mut gui = Gui::new("search");
    gui.queue("http://127.0.0.1:9/iso/ubuntu-24.04-desktop.iso", "ubuntu-24.04-desktop.iso");
    gui.queue("http://127.0.0.1:9/iso/ubuntu-24.04-server.iso", "ubuntu-24.04-server.iso");
    gui.queue("http://127.0.0.1:9/handbook/manual.pdf", "manual.pdf");
    gui.until("the rows", 5, |h| present(h, "manual.pdf"));
    let search = |gui: &mut Gui, text: &str| {
        let field = gui.h.get_all_by_role(Role::TextInput).next().unwrap();
        field.focus();
        gui.steps(2);
        gui.h.get_all_by_role(Role::TextInput).next().unwrap().type_text(text);
        gui.steps(3);
    };
    assert!(!gui.has("Clear search"), "nothing to clear yet");

    // Two words: both must match, in any order.
    search(&mut gui, "SERVER ubuntu");
    assert!(gui.has("ubuntu-24.04-server.iso"), "the matching row stays, and is still labelled by its full name");
    assert!(!gui.has("ubuntu-24.04-desktop.iso") && !gui.has("manual.pdf"));
    assert!(gui.has("1 match"));

    // The × empties the field and everything is back.
    gui.click("Clear search");
    assert!(gui.has("ubuntu-24.04-desktop.iso") && gui.has("manual.pdf"));
    assert!(!gui.has("1 match"));

    // The address, the category and the status are searched too.
    search(&mut gui, "handbook");
    assert!(gui.has("manual.pdf") && !gui.has("ubuntu-24.04-server.iso"));
    gui.click("Clear search");
    search(&mut gui, "queued compressed");
    assert!(gui.has("2 matches") && gui.has("ubuntu-24.04-server.iso") && !gui.has("manual.pdf"));
    gui.click("Clear search");

    // Nothing matches: say so, and offer the way back.
    search(&mut gui, "no-such-file");
    assert!(gui.has("No match") && gui.has("No downloads match \"no-such-file\""));
    gui.click("Clear search");
    gui.until("the list to return", 5, |h| present(h, "manual.pdf"));
}

/// "Hide" on the progress window takes the window away without touching the
/// download, and it can be brought back.
#[test]
fn progress_window_hides_and_comes_back() {
    let mut gui = Gui::new("hide");
    let mut opts = ServerOptions::new(24 * MIB);
    opts.per_conn_rate = 256 * 1024;
    let server = gui.server(opts);
    let mut new = NewDownload::new(RequestSpec::new(server.url()));
    new.file_name = Some("slow.iso".into());
    new.connections = Some(Connections::Fixed(2));
    let id = gui.app().manager().add(new);
    gui.until("the download to run", 20, |h| h.state().manager().view(id).is_some_and(|v| v.d.downloaded > 0));

    let show_progress = |gui: &mut Gui| {
        gui.h.get_by_label("slow.iso").click_secondary();
        gui.steps(3);
        gui.click("Show progress");
    };
    show_progress(&mut gui);
    gui.until("the progress window", 5, |h| present(h, "Resume capability"));

    gui.click("Hide");
    gui.until("the window to go away", 5, |h| !present(h, "Resume capability"));
    let before = gui.app().manager().view(id).unwrap();
    assert_eq!(before.d.status, Status::Running, "hiding must not stop the download");
    gui.until("the download to keep going while hidden", 20, |h| h.state().manager().view(id).unwrap().d.downloaded > before.d.downloaded);

    show_progress(&mut gui);
    gui.until("the progress window again", 5, |h| present(h, "Resume capability"));
    gui.app().manager().pause(id);
}

/// The installer's last page (and Options) open the browser set-up guide.
#[test]
fn browser_setup_guide_opens_on_request() {
    let mut gui = Gui::new("browser");
    gui.app().manager().push_event(rdm_core::manager::Event::BrowserSetup);
    gui.until("the guide", 5, |h| present(h, "2.  Add the extension to your browser"));
    assert!(gui.has("Copy the folder path"));
    gui.click("Close");
    gui.until("the guide to close", 5, |h| !present(h, "2.  Add the extension to your browser"));

    gui.click("Options");
    gui.until("the options dialog", 5, |h| present(h, "Site logins"));
    gui.click("Set up the browser extension…");
    gui.until("the guide from Options", 5, |h| present(h, "2.  Add the extension to your browser"));
}

/// A download handed over by the browser or the clipboard must wait in the
/// Download File Info dialog until the user decides — never start by itself.
#[test]
fn captured_download_waits_for_the_user() {
    let mut gui = Gui::new("capture");
    let server = gui.server(ServerOptions::new(MIB));

    let request = rdm_core::manager::AddRequest { url: server.url(), ..Default::default() };
    gui.app().manager().push_event(rdm_core::manager::Event::AddRequest(request));
    gui.until("Download File Info", 10, |h| present(h, "Start Download"));

    // Leave it alone for a while, as a user who is busy elsewhere would.
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        gui.h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(gui.app().manager().views().is_empty(), "the download was added without anyone confirming it");
    assert!(gui.has("Start Download"), "the dialog should still be waiting");

    // Cancel discards it.
    gui.click("Cancel");
    gui.until("the dialog to close", 5, |h| !present(h, "Start Download"));
    assert!(gui.app().manager().views().is_empty());
}

/// Ctrl+V with a link on the clipboard opens the Download File Info dialog for it.
#[test]
fn pasting_a_link_opens_the_add_dialog() {
    let mut gui = Gui::new("paste");
    let server = gui.server(ServerOptions::new(MIB));

    // Text that is not a link is ignored.
    gui.h.event(egui::Event::Paste("just some words".into()));
    gui.steps(5);
    assert!(!gui.has("Getting file information…") && !gui.has("Start Download"));

    gui.h.event(egui::Event::Paste(format!("  {}\n", server.url())));
    gui.until("Download File Info", 10, |h| present(h, "Start Download"));
    gui.click("Download Later");
    gui.until("the download to be queued", 5, |h| h.state().manager().views().len() == 1);
    let v = gui.app().manager().views().remove(0);
    assert_eq!((v.d.status, v.d.queue), (Status::Queued, Some(MAIN_QUEUE)));
}
