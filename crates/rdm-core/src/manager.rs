//! Everything above a single download: the list, categories, queues, the
//! scheduler and persistence. The GUI talks only to [`Manager`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike};
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::category::{self, Category};
use crate::filename::{self, part_path, state_path};
use crate::http::{self, build_client, NetConfig, Probe, RemoteInfo, RequestSpec};
use crate::limiter::RateLimiter;
use crate::segment::SegSnapshot;
use crate::state::{load_json, save_json, ResumeState};
use crate::task::{self, Connections, Outcome, Progress, Start, TaskConfig};

pub type Id = u64;
pub const MAIN_QUEUE: Id = 1;
pub const DEFAULT_API_PORT: u16 = 47821;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Added but not started (waiting in a queue or for the user).
    Queued,
    Running,
    Paused,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SiteLogin {
    pub host: String,
    pub user: String,
    pub password: String,
}

/// "Use at most N connections for this site."
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SiteLimit {
    pub host: String,
    pub connections: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub categories: Vec<Category>,
    pub connections: Connections,
    pub max_connections: u32,
    /// Bytes per second; applies only while `limiter_enabled`.
    pub speed_limit: u64,
    pub limiter_enabled: bool,
    pub net: NetConfig,
    pub max_retries: u32,
    pub stall_timeout_secs: u64,
    pub show_progress_window: bool,
    pub show_complete_dialog: bool,
    pub set_file_time: bool,
    pub monitor_clipboard: bool,
    /// Extensions the browser extension and clipboard monitor react to.
    pub capture_extensions: Vec<String>,
    pub api_port: u16,
    pub dark_mode: bool,
    pub site_logins: Vec<SiteLogin>,
    pub site_limits: Vec<SiteLimit>,
}

impl Default for Config {
    fn default() -> Self {
        let downloads = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));
        let categories = category::defaults(&downloads);
        let capture_extensions = categories.iter().flat_map(|c| c.extensions.iter().cloned()).collect();
        Self {
            categories,
            connections: Connections::Auto,
            max_connections: task::MAX_CONNECTIONS,
            speed_limit: 1024 * 1024,
            limiter_enabled: false,
            net: NetConfig::default(),
            max_retries: 20,
            stall_timeout_secs: 30,
            show_progress_window: true,
            show_complete_dialog: true,
            set_file_time: true,
            monitor_clipboard: false,
            capture_extensions,
            api_port: DEFAULT_API_PORT,
            dark_mode: false,
            site_logins: Vec::new(),
            site_limits: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    pub id: Id,
    pub spec: RequestSpec,
    pub file_name: String,
    pub dir: PathBuf,
    pub category: String,
    #[serde(default)]
    pub queue: Option<Id>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub downloaded: u64,
    pub status: Status,
    /// `None` until the server has been asked.
    #[serde(default)]
    pub resumable: Option<bool>,
    /// Per-download override of the global connection setting.
    #[serde(default)]
    pub connections: Option<Connections>,
    #[serde(default)]
    pub speed_limit: u64,
    pub added_at: DateTime<Local>,
    #[serde(default)]
    pub last_try_at: Option<DateTime<Local>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Local>>,
    #[serde(default)]
    pub error: Option<String>,
    /// The name was chosen by the user or already confirmed by the server;
    /// otherwise it is a guess from the URL that the first response may correct.
    #[serde(default = "yes")]
    pub name_final: bool,
    /// Category and folder follow the file name automatically.
    #[serde(default)]
    pub auto_place: bool,
}

fn yes() -> bool {
    true
}

impl Download {
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.file_name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostAction {
    #[default]
    Nothing,
    ExitApp,
    Sleep,
    Hibernate,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub start_enabled: bool,
    pub start_hour: u32,
    pub start_minute: u32,
    /// Monday first. All false with `start_enabled` means every day.
    pub days: [bool; 7],
    pub stop_enabled: bool,
    pub stop_hour: u32,
    pub stop_minute: u32,
    pub on_complete: PostAction,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            start_enabled: false,
            start_hour: 23,
            start_minute: 0,
            days: [false; 7],
            stop_enabled: false,
            stop_hour: 7,
            stop_minute: 0,
            on_complete: PostAction::Nothing,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Queue {
    pub id: Id,
    pub name: String,
    pub max_concurrent: u32,
    #[serde(default)]
    pub schedule: Schedule,
    /// Runtime only: queues never auto-run after a restart.
    #[serde(skip)]
    pub running: bool,
}

/// What the GUI shows for one row: the stored record plus live numbers.
#[derive(Debug, Clone)]
pub struct View {
    pub d: Download,
    pub speed: u64,
    pub connections: u32,
    /// Transient note while running, e.g. "connecting" or the error being retried.
    pub note: Option<String>,
}

/// A request to add a download that came from outside the GUI.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AddRequest {
    pub url: String,
    pub referer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    pub file_name: Option<String>,
    pub dir: Option<PathBuf>,
    /// Add without showing the Download File Info dialog.
    pub silent: bool,
    /// With `silent`: queue it instead of starting right away.
    pub queue_only: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    Finished { id: Id, ok: bool },
    AddRequest(AddRequest),
    QueueFinished { queue: Id, action: PostAction },
    StartQueue(Id),
    /// Another instance was launched: bring the window to the front.
    ShowWindow,
    Quit,
}

pub struct NewDownload {
    pub spec: RequestSpec,
    /// `None`: take it from the server (or the URL until the server answers).
    pub file_name: Option<String>,
    /// `None`: the category's folder.
    pub dir: Option<PathBuf>,
    /// `None`: by file extension.
    pub category: Option<String>,
    pub queue: Option<Id>,
    pub description: String,
    pub start: bool,
    /// An already-open first response, if the caller probed.
    pub probe: Option<Probe>,
    pub connections: Option<Connections>,
    pub speed_limit: u64,
}

impl NewDownload {
    pub fn new(spec: RequestSpec) -> Self {
        Self {
            spec,
            file_name: None,
            dir: None,
            category: None,
            queue: None,
            description: String::new(),
            start: true,
            probe: None,
            connections: None,
            speed_limit: 0,
        }
    }
}

enum ProbeSlot {
    Pending,
    Ready(Box<Probe>),
    Failed(String),
    Taken,
}

/// A probe running in the background; poll it from the UI thread.
#[derive(Clone)]
pub struct ProbeTicket {
    slot: Arc<Mutex<ProbeSlot>>,
    info: Arc<Mutex<Option<RemoteInfo>>>,
}

impl ProbeTicket {
    /// `None` while pending; then the server's description of the file, or the error.
    pub fn result(&self) -> Option<Result<RemoteInfo, String>> {
        match &*self.slot.lock() {
            ProbeSlot::Pending => None,
            ProbeSlot::Failed(e) => Some(Err(e.clone())),
            ProbeSlot::Ready(_) | ProbeSlot::Taken => self.info.lock().clone().map(Ok),
        }
    }

    /// Hand the open response over to the download (once).
    pub fn take(&self) -> Option<Probe> {
        let mut slot = self.slot.lock();
        match std::mem::replace(&mut *slot, ProbeSlot::Taken) {
            ProbeSlot::Ready(p) => Some(*p),
            other => {
                *slot = other;
                None
            }
        }
    }
}

struct Live {
    /// `None` while still asking the server about the file.
    progress: Option<Arc<Progress>>,
    cancel: watch::Sender<bool>,
    limiter: Arc<RateLimiter>,
    /// Set when the user removed the download while it was running.
    remove_after: Option<bool>,
}

struct State {
    config: Config,
    downloads: Vec<Download>,
    queues: Vec<Queue>,
    live: HashMap<Id, Live>,
    next_id: Id,
    dirty: bool,
    events: Vec<Event>,
    /// Per running queue: members already given their turn in this run.
    attempted: HashMap<Id, HashSet<Id>>,
    /// (queue, is_start) → day it last fired, so a schedule fires once per day.
    fired: HashMap<(Id, bool), NaiveDate>,
}

struct Inner {
    state: Mutex<State>,
    client: Mutex<Client>,
    global_limiter: Arc<RateLimiter>,
    data_dir: PathBuf,
    notify: Box<dyn Fn() + Send + Sync>,
    /// `Some` until drop.
    rt: Option<tokio::runtime::Runtime>,
}

impl Inner {
    fn rt(&self) -> &tokio::runtime::Runtime {
        self.rt.as_ref().expect("runtime lives as long as the manager")
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last reference may be released on one of the runtime's own
        // threads, where a blocking runtime shutdown would panic.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

/// A handle that does not keep the manager alive (for background servers).
#[derive(Clone)]
pub struct WeakManager {
    inner: Weak<Inner>,
}

impl WeakManager {
    pub fn upgrade(&self) -> Option<Manager> {
        self.inner.upgrade().map(|inner| Manager { inner })
    }
}

impl Manager {
    /// Load state from `data_dir` and start the background loops. `notify` is
    /// called (from any thread) when something the UI shows has changed.
    pub fn new(data_dir: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> crate::Result<Self> {
        let _ = std::fs::create_dir_all(&data_dir);
        let config: Config = load_json(&data_dir.join("config.json")).unwrap_or_default();
        let mut downloads: Vec<Download> = load_json(&data_dir.join("downloads.json")).unwrap_or_default();
        let mut queues: Vec<Queue> = load_json(&data_dir.join("queues.json")).unwrap_or_default();
        if !queues.iter().any(|q| q.id == MAIN_QUEUE) {
            queues.insert(
                0,
                Queue { id: MAIN_QUEUE, name: "Main download queue".into(), max_concurrent: 1, schedule: Schedule::default(), running: false },
            );
        }
        for d in &mut downloads {
            // The app was closed (or crashed) mid-transfer; the data on disk is what counts.
            if d.status == Status::Running {
                d.status = Status::Paused;
            }
            if d.status != Status::Complete {
                if let Some(st) = ResumeState::load(&state_path(&d.path())) {
                    d.downloaded = st.downloaded();
                    d.size = Some(st.size);
                }
            }
        }
        let next_id = downloads.iter().map(|d| d.id).chain(queues.iter().map(|q| q.id)).max().unwrap_or(MAIN_QUEUE) + 1;

        let client = build_client(&config.net)?;
        let global_limiter = Arc::new(RateLimiter::new(if config.limiter_enabled { config.speed_limit } else { 0 }));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("rdm-net")
            .build()
            .map_err(crate::Error::Io)?;

        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                config,
                downloads,
                queues,
                live: HashMap::new(),
                next_id,
                dirty: false,
                events: Vec::new(),
                attempted: HashMap::new(),
                fired: HashMap::new(),
            }),
            client: Mutex::new(client),
            global_limiter,
            data_dir,
            notify: Box::new(notify),
            rt: Some(rt),
        });

        let weak = Arc::downgrade(&inner);
        inner.rt().spawn(housekeeping(weak));
        Ok(Self { inner })
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        self.inner.rt().handle()
    }

    pub fn downgrade(&self) -> WeakManager {
        WeakManager { inner: Arc::downgrade(&self.inner) }
    }

    /// Start the loopback API used by the browser extension and by second
    /// instances of the app. Returns an error if the port is taken.
    pub fn start_api(&self) -> std::io::Result<u16> {
        let port = self.config().api_port;
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        let weak = self.downgrade();
        let _guard = self.inner.rt().enter();
        let listener = tokio::net::TcpListener::from_std(listener)?;
        self.inner.rt().spawn(crate::api::serve(listener, weak));
        Ok(port)
    }

    // ---- Reading -------------------------------------------------------

    pub fn views(&self) -> Vec<View> {
        let st = self.inner.state.lock();
        st.downloads
            .iter()
            .filter(|d| st.live.get(&d.id).is_none_or(|l| l.remove_after.is_none()))
            .map(|d| view_of(d, st.live.get(&d.id)))
            .collect()
    }

    pub fn view(&self, id: Id) -> Option<View> {
        let st = self.inner.state.lock();
        st.downloads.iter().find(|d| d.id == id).map(|d| view_of(d, st.live.get(&id)))
    }

    pub fn segments(&self, id: Id) -> Vec<SegSnapshot> {
        let progress = self.inner.state.lock().live.get(&id).and_then(|l| l.progress.clone());
        progress.map(|p| p.segments()).unwrap_or_default()
    }

    pub fn active_count(&self) -> usize {
        self.inner.state.lock().live.len()
    }

    pub fn config(&self) -> Config {
        self.inner.state.lock().config.clone()
    }

    pub fn queues(&self) -> Vec<Queue> {
        self.inner.state.lock().queues.clone()
    }

    pub fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut self.inner.state.lock().events)
    }

    pub fn push_event(&self, event: Event) {
        self.inner.state.lock().events.push(event);
        (self.inner.notify)();
    }

    // ---- Adding --------------------------------------------------------

    /// Ask the server about a URL without committing to a download yet.
    pub fn probe(&self, spec: RequestSpec) -> ProbeTicket {
        let ticket = ProbeTicket { slot: Arc::new(Mutex::new(ProbeSlot::Pending)), info: Arc::new(Mutex::new(None)) };
        let (t, inner) = (ticket.clone(), self.inner.clone());
        self.inner.rt().spawn(async move {
            let spec = inner.with_site_login(spec);
            let client = inner.client.lock().clone();
            let res = tokio::time::timeout(Duration::from_secs(30), http::probe(&client, &spec)).await;
            let slot = match res {
                Ok(Ok(p)) => {
                    *t.info.lock() = Some(p.info.clone());
                    ProbeSlot::Ready(Box::new(p))
                }
                Ok(Err(e)) => ProbeSlot::Failed(e.to_string()),
                Err(_) => ProbeSlot::Failed("timed out waiting for the server".into()),
            };
            *t.slot.lock() = slot;
            (inner.notify)();
        });
        ticket
    }

    /// Where a file of this name would be saved by default.
    pub fn default_placement(&self, file_name: &str) -> (String, PathBuf) {
        let st = self.inner.state.lock();
        place(&st.config, file_name)
    }

    pub fn add(&self, new: NewDownload) -> Id {
        let id;
        {
            let mut st = self.inner.state.lock();
            let name_final = new.file_name.is_some() || new.probe.is_some();
            let name = new
                .file_name
                .map(|n| filename::sanitize(&n))
                .filter(|n| !n.is_empty())
                .or_else(|| new.probe.as_ref().map(|p| p.info.file_name.clone()))
                .or_else(|| filename::from_url(&new.spec.url))
                .unwrap_or_else(|| "download".into());
            let auto_place = new.dir.is_none() && new.category.is_none();
            let (auto_cat, auto_dir) = place(&st.config, &name);
            let category = new.category.unwrap_or(auto_cat);
            let dir = new.dir.unwrap_or_else(|| {
                category::dir_for(&category, &st.config.categories).map(Path::to_path_buf).unwrap_or(auto_dir)
            });
            let path = unique_among(&st, &dir, &name, None);
            let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);

            id = st.next_id;
            st.next_id += 1;
            let info = new.probe.as_ref().map(|p| &p.info);
            st.downloads.push(Download {
                id,
                spec: new.spec,
                file_name,
                dir,
                category,
                queue: new.queue,
                description: new.description,
                size: info.and_then(|i| i.size),
                downloaded: 0,
                status: Status::Queued,
                resumable: info.map(|i| i.resumable),
                connections: new.connections,
                speed_limit: new.speed_limit,
                added_at: Local::now(),
                last_try_at: None,
                completed_at: None,
                error: None,
                name_final,
                auto_place,
            });
            st.dirty = true;
        }
        if new.start {
            self.inner.clone().start(id, new.probe);
        }
        (self.inner.notify)();
        id
    }

    // ---- Controlling ---------------------------------------------------

    pub fn start(&self, id: Id) {
        self.inner.clone().start(id, None);
        (self.inner.notify)();
    }

    pub fn pause(&self, id: Id) {
        let st = self.inner.state.lock();
        if let Some(live) = st.live.get(&id) {
            let _ = live.cancel.send(true);
        }
    }

    pub fn pause_all(&self) {
        let mut st = self.inner.state.lock();
        for q in &mut st.queues {
            q.running = false;
        }
        st.attempted.clear();
        for live in st.live.values() {
            let _ = live.cancel.send(true);
        }
    }

    /// Remove from the list. In-progress data is always deleted;
    /// `delete_file` also deletes a completed file from disk.
    pub fn remove(&self, id: Id, delete_file: bool) {
        let mut st = self.inner.state.lock();
        if let Some(live) = st.live.get_mut(&id) {
            // Still running: let it wind down, then `finish` completes the removal.
            live.remove_after = Some(delete_file);
            let _ = live.cancel.send(true);
            return;
        }
        if let Some(pos) = st.downloads.iter().position(|d| d.id == id) {
            let d = st.downloads.remove(pos);
            delete_files(&d, delete_file);
            st.dirty = true;
        }
        drop(st);
        (self.inner.notify)();
    }

    /// Throw away what was downloaded and fetch the file again.
    pub fn restart(&self, id: Id) {
        {
            let mut st = self.inner.state.lock();
            if st.live.contains_key(&id) {
                return;
            }
            let Some(d) = st.downloads.iter_mut().find(|d| d.id == id) else { return };
            delete_files(d, true);
            d.downloaded = 0;
            d.status = Status::Queued;
            d.error = None;
            d.completed_at = None;
            st.dirty = true;
        }
        self.start(id);
    }

    pub fn set_speed_limit(&self, id: Id, rate: u64) {
        let mut st = self.inner.state.lock();
        if let Some(live) = st.live.get(&id) {
            live.limiter.set_rate(rate);
        }
        if let Some(d) = st.downloads.iter_mut().find(|d| d.id == id) {
            d.speed_limit = rate;
            st.dirty = true;
        }
    }

    pub fn set_description(&self, id: Id, text: String) {
        let mut st = self.inner.state.lock();
        if let Some(d) = st.downloads.iter_mut().find(|d| d.id == id) {
            d.description = text;
            st.dirty = true;
        }
    }

    /// Point a download at a new address (expired link) and keep its progress.
    pub fn set_url(&self, id: Id, url: String) {
        let mut st = self.inner.state.lock();
        if st.live.contains_key(&id) {
            return;
        }
        let Some(d) = st.downloads.iter_mut().find(|d| d.id == id) else { return };
        // The resume file is keyed by URL so that a different file is never spliced in.
        let state_file = state_path(&d.path());
        if let Some(mut saved) = ResumeState::load(&state_file) {
            saved.url = url.clone();
            let _ = save_json(&state_file, &saved);
        }
        d.spec.url = url;
        d.error = None;
        if d.status == Status::Failed {
            d.status = Status::Paused;
        }
        st.dirty = true;
    }

    pub fn set_config(&self, config: Config) -> crate::Result<()> {
        let mut st = self.inner.state.lock();
        if st.config.net != config.net {
            *self.inner.client.lock() = build_client(&config.net)?;
        }
        self.inner.global_limiter.set_rate(if config.limiter_enabled { config.speed_limit } else { 0 });
        st.config = config;
        st.dirty = true;
        Ok(())
    }

    // ---- Queues --------------------------------------------------------

    pub fn add_queue(&self, name: String) -> Id {
        let mut st = self.inner.state.lock();
        let id = st.next_id;
        st.next_id += 1;
        st.queues.push(Queue { id, name, max_concurrent: 1, schedule: Schedule::default(), running: false });
        st.dirty = true;
        id
    }

    pub fn update_queue(&self, queue: Queue) {
        let mut st = self.inner.state.lock();
        if let Some(q) = st.queues.iter_mut().find(|q| q.id == queue.id) {
            let running = q.running;
            *q = queue;
            q.running = running;
            q.max_concurrent = q.max_concurrent.clamp(1, 10);
            st.dirty = true;
        }
    }

    pub fn remove_queue(&self, id: Id) {
        if id == MAIN_QUEUE {
            return;
        }
        let mut st = self.inner.state.lock();
        st.queues.retain(|q| q.id != id);
        for d in st.downloads.iter_mut().filter(|d| d.queue == Some(id)) {
            d.queue = None;
        }
        st.attempted.remove(&id);
        st.dirty = true;
    }

    pub fn set_queue(&self, id: Id, queue: Option<Id>) {
        let mut st = self.inner.state.lock();
        if let Some(d) = st.downloads.iter_mut().find(|d| d.id == id) {
            d.queue = queue;
            st.dirty = true;
        }
    }

    pub fn start_queue(&self, id: Id) {
        let mut st = self.inner.state.lock();
        st.attempted.insert(id, HashSet::new());
        if let Some(q) = st.queues.iter_mut().find(|q| q.id == id) {
            q.running = true;
        }
        drop(st);
        self.inner.clone().run_queues();
    }

    pub fn stop_queue(&self, id: Id) {
        let mut st = self.inner.state.lock();
        if let Some(q) = st.queues.iter_mut().find(|q| q.id == id) {
            q.running = false;
        }
        st.attempted.remove(&id);
        let members: Vec<Id> = st.downloads.iter().filter(|d| d.queue == Some(id)).map(|d| d.id).collect();
        for m in members {
            if let Some(live) = st.live.get(&m) {
                let _ = live.cancel.send(true);
            }
        }
    }

    // ---- Lifecycle -----------------------------------------------------

    pub fn save_now(&self) {
        self.inner.save();
    }

    /// Pause everything, wait for the data to reach the disk, save the list.
    pub fn shutdown(&self) {
        self.pause_all();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while self.active_count() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        self.inner.save();
    }
}

fn view_of(d: &Download, live: Option<&Live>) -> View {
    let mut v = View { d: d.clone(), speed: 0, connections: 0, note: None };
    if let Some(live) = live {
        match &live.progress {
            Some(p) => {
                v.d.downloaded = p.downloaded();
                if let Some(size) = p.size() {
                    v.d.size = Some(size);
                }
                v.speed = p.speed();
                v.connections = p.connections();
                v.note = p.last_error().map(|e| format!("Retrying: {e}"));
                if v.note.is_none() && v.connections == 0 {
                    v.note = Some("Connecting…".into());
                }
            }
            None => v.note = Some("Connecting…".into()),
        }
    }
    v
}

/// Category and folder a file name maps to under the current settings.
fn place(config: &Config, file_name: &str) -> (String, PathBuf) {
    let cat = category::categorize(file_name, &config.categories).to_string();
    let dir = category::dir_for(&cat, &config.categories)
        .or_else(|| category::dir_for(category::GENERAL, &config.categories))
        .map(Path::to_path_buf)
        .or_else(dirs::download_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    (cat, dir)
}

/// A path no file on disk and no other unfinished download is using.
fn unique_among(st: &State, dir: &Path, name: &str, except: Option<Id>) -> PathBuf {
    filename::unique_path(dir, name, |p| {
        st.downloads.iter().any(|d| Some(d.id) != except && d.status != Status::Complete && d.path() == p)
    })
}

fn delete_files(d: &Download, delete_final: bool) {
    let path = d.path();
    let _ = std::fs::remove_file(part_path(&path));
    let _ = std::fs::remove_file(state_path(&path));
    if delete_final && d.status == Status::Complete {
        let _ = std::fs::remove_file(&path);
    }
}

impl Inner {
    fn with_site_login(&self, mut spec: RequestSpec) -> RequestSpec {
        if spec.auth.is_none() {
            if let Some(host) = url::Url::parse(&spec.url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)) {
                let st = self.state.lock();
                if let Some(login) = st.config.site_logins.iter().find(|l| host_matches(&host, &l.host)) {
                    spec.auth = Some((login.user.clone(), login.password.clone()));
                }
            }
        }
        spec
    }

    fn start(self: Arc<Self>, id: Id, probe: Option<Probe>) {
        let (cancel, cancel_rx) = watch::channel(false);
        {
            let mut st = self.state.lock();
            if st.live.contains_key(&id) {
                return;
            }
            let Some(d) = st.downloads.iter_mut().find(|d| d.id == id) else { return };
            if d.status == Status::Complete {
                return;
            }
            d.status = Status::Running;
            d.error = None;
            d.last_try_at = Some(Local::now());
            let limiter = Arc::new(RateLimiter::new(d.speed_limit));
            st.live.insert(id, Live { progress: None, cancel: cancel.clone(), limiter, remove_after: None });
            st.dirty = true;
        }
        let me = self.clone();
        self.rt().spawn(async move {
            let (outcome, progress) = me.clone().run_download(id, probe, cancel, cancel_rx).await;
            me.finish(id, outcome, progress);
        });
    }

    async fn run_download(
        self: Arc<Self>,
        id: Id,
        probe: Option<Probe>,
        cancel: watch::Sender<bool>,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> (Outcome, Option<Arc<Progress>>) {
        let (spec, path, limiter, cfg) = {
            let st = self.state.lock();
            let Some(d) = st.downloads.iter().find(|d| d.id == id) else {
                return (Outcome::Failed("download no longer exists".into()), None);
            };
            let limiter = st.live.get(&id).map(|l| l.limiter.clone()).unwrap_or_else(|| Arc::new(RateLimiter::unlimited()));
            (d.spec.clone(), d.path(), limiter, st.config.clone())
        };
        let spec = self.with_site_login(spec);
        let client = self.client.lock().clone();
        let stall = Duration::from_secs(cfg.stall_timeout_secs.max(5));

        // Resume if the data from an earlier run is there and belongs to this URL.
        let saved = ResumeState::load(&state_path(&path)).filter(|s| s.url == spec.url && part_path(&path).exists());
        let (start, path) = match saved {
            Some(state) => (Start::Resume(state), path),
            None => {
                let probe = match probe {
                    Some(p) => p,
                    None => {
                        let res = task::probe_with_retry(&client, &spec, cfg.max_retries, stall, &mut cancel_rx, |_| {}).await;
                        match res {
                            Ok(p) => p,
                            Err(outcome) => return (outcome, None),
                        }
                    }
                };
                // The server may know a better name than the URL suggested.
                let mut st = self.state.lock();
                let st = &mut *st;
                let mut path = path;
                if let Some(pos) = st.downloads.iter().position(|d| d.id == id) {
                    let (name_final, auto_place) = (st.downloads[pos].name_final, st.downloads[pos].auto_place);
                    if !name_final {
                        let name = probe.info.file_name.clone();
                        let (mut cat, mut dir) = (st.downloads[pos].category.clone(), st.downloads[pos].dir.clone());
                        if auto_place {
                            (cat, dir) = place(&st.config, &name);
                        }
                        let unique = filename::unique_path(&dir, &name, |p| {
                            st.downloads.iter().any(|o| o.id != id && o.status != Status::Complete && o.path() == p)
                        });
                        let d = &mut st.downloads[pos];
                        d.file_name = unique.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
                        d.category = cat;
                        d.dir = dir;
                        d.name_final = true;
                        path = d.path();
                    }
                    let d = &mut st.downloads[pos];
                    d.size = probe.info.size;
                    d.resumable = Some(probe.info.resumable);
                    st.dirty = true;
                }
                (Start::Fresh(probe), path)
            }
        };

        let host = url::Url::parse(&spec.url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase));
        let site_limit = host.and_then(|h| cfg.site_limits.iter().find(|l| host_matches(&h, &l.host)).map(|l| l.connections));
        let per_download = self.state.lock().downloads.iter().find(|d| d.id == id).and_then(|d| d.connections);

        let mut tcfg = TaskConfig::new(spec, path);
        tcfg.connections = match (site_limit, per_download) {
            (Some(n), _) => Connections::Fixed(n.clamp(1, task::MAX_CONNECTIONS)),
            (None, Some(c)) => c,
            (None, None) => cfg.connections,
        };
        tcfg.max_connections = cfg.max_connections;
        tcfg.max_retries = cfg.max_retries;
        tcfg.stall_timeout = stall;
        tcfg.limiter = limiter;
        tcfg.global_limiter = self.global_limiter.clone();
        tcfg.set_file_time = cfg.set_file_time;

        let handle = task::spawn_with_cancel(client, tcfg, start, cancel, cancel_rx);
        let progress = handle.progress.clone();
        if let Some(live) = self.state.lock().live.get_mut(&id) {
            live.progress = Some(progress.clone());
        }
        (self.notify)();
        (handle.wait().await, Some(progress))
    }

    fn finish(&self, id: Id, outcome: Outcome, progress: Option<Arc<Progress>>) {
        {
            let mut st = self.state.lock();
            let live = st.live.remove(&id);
            let Some(pos) = st.downloads.iter().position(|d| d.id == id) else { return };

            if let Some(delete_file) = live.and_then(|l| l.remove_after) {
                let mut d = st.downloads.remove(pos);
                if outcome == Outcome::Complete {
                    d.status = Status::Complete;
                }
                delete_files(&d, delete_file);
            } else {
                let d = &mut st.downloads[pos];
                if let Some(p) = &progress {
                    d.downloaded = p.downloaded();
                    if let Some(size) = p.size() {
                        d.size = Some(size);
                    }
                    d.resumable = Some(p.resumable());
                }
                match outcome {
                    Outcome::Complete => {
                        d.status = Status::Complete;
                        d.completed_at = Some(Local::now());
                        d.error = None;
                        if let Some(size) = d.size {
                            d.downloaded = size;
                        }
                    }
                    Outcome::Paused => d.status = Status::Paused,
                    Outcome::Failed(msg) => {
                        d.status = Status::Failed;
                        d.error = Some(msg);
                    }
                }
                let ok = d.status == Status::Complete;
                st.events.push(Event::Finished { id, ok });
            }
            st.dirty = true;
        }
        (self.notify)();
    }

    /// Give running queues their next files; notice queues that have finished.
    fn run_queues(self: Arc<Self>) {
        let mut to_start = Vec::new();
        {
            let mut st = self.state.lock();
            let st = &mut *st;
            for q in st.queues.iter_mut().filter(|q| q.running) {
                let attempted = st.attempted.entry(q.id).or_default();
                let members: Vec<&Download> = st.downloads.iter().filter(|d| d.queue == Some(q.id)).collect();
                let mut active = members.iter().filter(|d| st.live.contains_key(&d.id)).count();
                // Each member gets one turn per queue run, so a file that keeps
                // failing cannot hold the queue forever.
                let waiting: Vec<Id> = members
                    .iter()
                    .filter(|d| !st.live.contains_key(&d.id) && d.status != Status::Complete && !attempted.contains(&d.id))
                    .map(|d| d.id)
                    .collect();
                for next in waiting {
                    if active >= q.max_concurrent.max(1) as usize {
                        break;
                    }
                    attempted.insert(next);
                    to_start.push(next);
                    active += 1;
                }
                if active == 0 {
                    q.running = false;
                    st.attempted.remove(&q.id);
                    st.events.push(Event::QueueFinished { queue: q.id, action: q.schedule.on_complete });
                }
            }
        }
        if !to_start.is_empty() {
            for id in to_start {
                self.clone().start(id, None);
            }
            (self.notify)();
        }
    }

    /// Fire schedule start/stop times. Called once a second.
    fn run_schedules(self: &Arc<Self>) {
        let now = Local::now();
        let today = now.date_naive();
        let weekday = now.weekday().num_days_from_monday() as usize;
        let minute_now = (now.hour(), now.minute());
        let mut actions = Vec::new();
        {
            let mut st = self.state.lock();
            let st = &mut *st;
            for q in &st.queues {
                let s = &q.schedule;
                let day_ok = s.days.iter().all(|d| !d) || s.days[weekday];
                if s.start_enabled && day_ok && minute_now == (s.start_hour, s.start_minute) && st.fired.insert((q.id, true), today) != Some(today) {
                    actions.push((q.id, true));
                }
                if s.stop_enabled && minute_now == (s.stop_hour, s.stop_minute) && st.fired.insert((q.id, false), today) != Some(today) {
                    actions.push((q.id, false));
                }
            }
        }
        let manager = Manager { inner: self.clone() };
        for (queue, start) in actions {
            if start {
                manager.start_queue(queue);
            } else {
                manager.stop_queue(queue);
            }
        }
    }

    fn save(&self) {
        let (config, downloads, queues) = {
            let mut st = self.state.lock();
            st.dirty = false;
            // Persist live progress so a crash loses as little of the display as possible.
            let downloads: Vec<Download> = st.downloads.iter().map(|d| view_of(d, st.live.get(&d.id)).d).collect();
            (st.config.clone(), downloads, st.queues.clone())
        };
        for (name, res) in [
            ("config.json", save_json(&self.data_dir.join("config.json"), &config)),
            ("downloads.json", save_json(&self.data_dir.join("downloads.json"), &downloads)),
            ("queues.json", save_json(&self.data_dir.join("queues.json"), &queues)),
        ] {
            if let Err(e) = res {
                log::error!("cannot save {name}: {e}");
            }
        }
    }
}

/// `host` is `pattern` or a subdomain of it.
fn host_matches(host: &str, pattern: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("*.").to_ascii_lowercase();
    !pattern.is_empty() && (host == pattern || host.ends_with(&format!(".{pattern}")))
}

/// Once a second: advance queues and schedules; save when something changed.
async fn housekeeping(weak: Weak<Inner>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut since_save = 0u32;
    loop {
        tick.tick().await;
        let Some(inner) = weak.upgrade() else { return };
        inner.clone().run_queues();
        inner.run_schedules();
        since_save += 1;
        let (dirty, active) = {
            let st = inner.state.lock();
            (st.dirty, !st.live.is_empty())
        };
        // Changes are saved promptly; progress of running downloads every 10 s.
        if (dirty && since_save >= 2) || (active && since_save >= 10) {
            since_save = 0;
            let inner = inner.clone();
            let _ = tokio::task::spawn_blocking(move || inner.save()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_patterns() {
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("cdn.example.com", "example.com"));
        assert!(host_matches("cdn.example.com", "*.example.com"));
        assert!(!host_matches("notexample.com", "example.com"));
        assert!(!host_matches("example.com", ""));
    }
}
