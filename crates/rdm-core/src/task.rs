//! One download: a supervisor that keeps the right number of connections
//! busy, and the workers that each drive one connection.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use parking_lot::Mutex;
use reqwest::header;
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use crate::error::{describe, Error};
use crate::filename::{part_path, state_path};
use crate::http::{self, parse_content_range, Probe, RequestSpec};
use crate::limiter::RateLimiter;
use crate::segment::{SegSnapshot, Segment, SegmentTable, UNKNOWN_END};
use crate::state::{save_json, ResumeState};
use crate::writer::{open_part_file, Writer, WriterHandle};

pub const DEFAULT_MIN_SEGMENT: u64 = 512 * 1024;
pub const MAX_CONNECTIONS: u32 = 32;
/// Where Auto starts before it measures whether more connections help.
pub const AUTO_START_CONNECTIONS: u32 = 8;

/// Network chunks are coalesced into buffers of this size before hitting the disk thread.
const WRITE_BUF: usize = 512 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(250);
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(3);
/// Auto mode: ramp-up time granted to new connections, then the measurement window.
const AUTO_SETTLE: Duration = Duration::from_millis(750);
const AUTO_MEASURE: Duration = Duration::from_millis(1250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connections {
    /// Start at 8 and add connections for as long as they add throughput.
    Auto,
    Fixed(u32),
}

#[derive(Clone)]
pub struct TaskConfig {
    pub spec: RequestSpec,
    /// Where the finished file goes. In-progress data lives next to it.
    pub final_path: PathBuf,
    pub connections: Connections,
    pub max_connections: u32,
    pub min_segment: u64,
    /// Consecutive failed rounds tolerated before giving up.
    pub max_retries: u32,
    pub stall_timeout: Duration,
    pub limiter: Arc<RateLimiter>,
    pub global_limiter: Arc<RateLimiter>,
    pub set_file_time: bool,
}

impl TaskConfig {
    pub fn new(spec: RequestSpec, final_path: PathBuf) -> Self {
        Self {
            spec,
            final_path,
            connections: Connections::Auto,
            max_connections: MAX_CONNECTIONS,
            min_segment: DEFAULT_MIN_SEGMENT,
            max_retries: 20,
            stall_timeout: Duration::from_secs(30),
            limiter: Arc::new(RateLimiter::unlimited()),
            global_limiter: Arc::new(RateLimiter::unlimited()),
            set_file_time: true,
        }
    }
}

pub enum Start {
    /// Probe not done yet; the task does it.
    New,
    Fresh(Probe),
    Resume(ResumeState),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Complete,
    Paused,
    Failed(String),
}

/// Live numbers, readable from any thread without touching the task.
pub struct Progress {
    size: AtomicU64,
    downloaded: AtomicU64,
    speed: AtomicU64,
    connections: AtomicU32,
    resumable: AtomicBool,
    table: Mutex<Option<Arc<SegmentTable>>>,
    info: Mutex<Option<http::RemoteInfo>>,
    last_error: Mutex<Option<String>>,
}

impl Progress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            size: AtomicU64::new(UNKNOWN_END),
            downloaded: AtomicU64::new(0),
            speed: AtomicU64::new(0),
            connections: AtomicU32::new(0),
            resumable: AtomicBool::new(false),
            table: Mutex::new(None),
            info: Mutex::new(None),
            last_error: Mutex::new(None),
        })
    }

    pub fn size(&self) -> Option<u64> {
        Some(self.size.load(Ordering::Relaxed)).filter(|&s| s != UNKNOWN_END)
    }
    pub fn downloaded(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }
    /// Smoothed bytes per second.
    pub fn speed(&self) -> u64 {
        self.speed.load(Ordering::Relaxed)
    }
    pub fn connections(&self) -> u32 {
        self.connections.load(Ordering::Relaxed)
    }
    pub fn resumable(&self) -> bool {
        self.resumable.load(Ordering::Relaxed)
    }
    pub fn segments(&self) -> Vec<SegSnapshot> {
        self.table.lock().as_ref().map(|t| t.snapshot()).unwrap_or_default()
    }
    /// What the server said about the file (available once connected).
    pub fn info(&self) -> Option<http::RemoteInfo> {
        self.info.lock().clone()
    }
    /// Most recent transient problem, for display while retrying.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }
}

pub struct TaskHandle {
    pub progress: Arc<Progress>,
    cancel: watch::Sender<bool>,
    join: JoinHandle<Outcome>,
}

/// Lets another owner pause a download whose handle is busy being awaited.
#[derive(Clone)]
pub struct Pauser(watch::Sender<bool>);

impl Pauser {
    pub fn pause(&self) {
        let _ = self.0.send(true);
    }
}

impl TaskHandle {
    /// Ask the download to stop; progress is kept. `wait` returns `Paused`.
    pub fn pause(&self) {
        let _ = self.cancel.send(true);
    }

    pub fn pauser(&self) -> Pauser {
        Pauser(self.cancel.clone())
    }

    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    /// Stop dead without flushing or saving, as a crash would. Only what the
    /// last periodic checkpoint recorded survives. For tests.
    #[doc(hidden)]
    pub fn kill(self) {
        self.join.abort();
    }

    pub async fn wait(self) -> Outcome {
        self.join.await.unwrap_or_else(|e| Outcome::Failed(format!("download task crashed: {e}")))
    }
}

/// Start a download on the current tokio runtime.
pub fn spawn(client: Client, cfg: TaskConfig, start: Start) -> TaskHandle {
    let (cancel, cancel_rx) = watch::channel(false);
    spawn_with_cancel(client, cfg, start, cancel, cancel_rx)
}

/// Like [`spawn`], but pausing is driven by a channel the caller already
/// holds (so one pause button covers the caller's own preparation too).
pub fn spawn_with_cancel(
    client: Client,
    cfg: TaskConfig,
    start: Start,
    cancel: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
) -> TaskHandle {
    let progress = Progress::new();
    let task = Task { client, cfg: Arc::new(cfg), progress: progress.clone(), cancel: cancel_rx };
    let join = tokio::spawn(task.run(start));
    TaskHandle { progress, cancel, join }
}

struct Task {
    client: Client,
    cfg: Arc<TaskConfig>,
    progress: Arc<Progress>,
    cancel: watch::Receiver<bool>,
}

enum Once {
    Done(Outcome),
    /// Throw away what we have and start over (`single`: without range requests).
    Restart { single: bool, reason: String },
}

#[derive(Debug)]
enum WorkerError {
    Cancelled,
    /// Dropped by the supervisor for being far slower than its peers.
    Evicted,
    /// Transient: try again after a back-off.
    Retry(String),
    Status(u16),
    /// The server turned this request away while it is serving (or was just
    /// serving) others: a connection or request-rate limit. `streak` is the
    /// number of consecutive rejected rounds, 1 for the first.
    Busy { code: u16, streak: u32 },
    /// The server ignored `Range`.
    RangeIgnored,
    /// The remote file is not the one we started downloading.
    Changed,
    Disk(String),
}

/// Shared by the supervisor and the workers of one attempt.
struct Run {
    client: Client,
    cfg: Arc<TaskConfig>,
    progress: Arc<Progress>,
    table: Arc<SegmentTable>,
    writer: WriterHandle,
    cancel: watch::Receiver<bool>,
    size: Option<u64>,
    /// Range requests may be used (segmenting and resuming are possible).
    ranged: bool,
    /// A 206 has been seen, so a later 200 means "changed", not "unsupported".
    range_proven: AtomicBool,
    validator: Option<String>,
    /// Redirect target, reused so later connections skip the redirect hops.
    url: Mutex<String>,
    /// Workers currently receiving a body.
    streaming: AtomicU32,
    /// Workers alive, and how many the supervisor currently wants.
    live: AtomicU32,
    target: AtomicU32,
    busy: Mutex<Busy>,
    /// The server has refused a request during this run. From then on new
    /// requests go out one at a time (`gate`), segments are claimed only when
    /// it is a connection's turn, and streaming connections absorb untouched
    /// neighbours instead of issuing new requests.
    pressure: AtomicBool,
    gate: tokio::sync::Mutex<()>,
    /// Connections waiting for, or making, a request under pressure.
    probers: AtomicU32,
}

struct ProberGuard<'a>(&'a AtomicU32);

impl Drop for ProberGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Back-pressure from the server (429/503 and friends).
struct Busy {
    /// No new request before this instant.
    quiet_until: Instant,
    last_reject: Option<Instant>,
    /// Consecutive rejected rounds; 0 when the last request was accepted.
    streak: u32,
}

impl Run {
    /// Record a rejection of a request sent at `sent_at`; returns the streak.
    fn note_busy(&self, sent_at: Instant, retry_after: Option<Duration>) -> u32 {
        self.pressure.store(true, Ordering::Relaxed);
        let mut b = self.busy.lock();
        let now = Instant::now();
        // Requests that were already in flight when the previous rejection
        // arrived belong to the same round and must not escalate the back-off.
        if b.last_reject.is_none_or(|t| sent_at >= t) {
            b.streak += 1;
        }
        b.last_reject = Some(now);
        let own = Duration::from_secs(1 << (b.streak - 1).min(5)).min(Duration::from_secs(30));
        let wait = retry_after.map_or(own, |ra| ra.clamp(own, Duration::from_secs(120)));
        b.quiet_until = b.quiet_until.max(now + wait);
        b.streak
    }

    fn note_accepted(&self) {
        let mut b = self.busy.lock();
        if b.streak != 0 {
            b.streak = 0;
            b.last_reject = None;
        }
    }
}

/// `Retry-After` as a delay; both the seconds and the HTTP-date forms.
fn retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    let v = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    (when.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().ok()
}

/// A background task that must not outlive the download that started it,
/// however that download ends (including being dropped mid-flight).
struct AbortOnDrop(JoinHandle<()>);

impl AbortOnDrop {
    async fn stop(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn cancelled(rx: &mut watch::Receiver<bool>) {
    // A dropped sender also counts: nobody is left to want the result.
    let _ = rx.wait_for(|c| *c).await;
}

fn backoff(attempt: u32) -> Duration {
    let base = 0.5 * 2f64.powi(attempt.saturating_sub(1).min(10) as i32);
    let jitter = 0.8 + fastrand::f64() * 0.4;
    Duration::from_secs_f64(base.min(30.0) * jitter)
}

impl Task {
    async fn run(mut self, start: Start) -> Outcome {
        let mut start = Some(start);
        let mut force_single = false;
        let mut restarts = 0u32;
        loop {
            let begin = match start.take() {
                Some(Start::New) | None => {
                    self.reset_progress();
                    match self.probe().await {
                        Ok(p) => Start::Fresh(p),
                        Err(outcome) => return outcome,
                    }
                }
                Some(s) => s,
            };
            match self.run_once(begin, force_single).await {
                Once::Done(outcome) => return outcome,
                Once::Restart { single, reason } => {
                    restarts += 1;
                    if restarts > self.cfg.max_retries.max(3) {
                        return Outcome::Failed(reason);
                    }
                    log::warn!("restarting {}: {reason}", self.cfg.spec.url);
                    *self.progress.last_error.lock() = Some(reason);
                    force_single |= single;
                    let _ = std::fs::remove_file(part_path(&self.cfg.final_path));
                    let _ = std::fs::remove_file(state_path(&self.cfg.final_path));
                }
            }
        }
    }

    fn reset_progress(&self) {
        self.progress.downloaded.store(0, Ordering::Relaxed);
        self.progress.speed.store(0, Ordering::Relaxed);
        self.progress.connections.store(0, Ordering::Relaxed);
        *self.progress.table.lock() = None;
    }

    async fn probe(&mut self) -> Result<Probe, Outcome> {
        let progress = self.progress.clone();
        probe_with_retry(&self.client, &self.cfg.spec, self.cfg.max_retries, self.cfg.stall_timeout, &mut self.cancel, |e| {
            *progress.last_error.lock() = Some(e)
        })
        .await
    }

    async fn run_once(&mut self, start: Start, force_single: bool) -> Once {
        let cfg = self.cfg.clone();
        let part = part_path(&cfg.final_path);
        let state_file = state_path(&cfg.final_path);
        let min_seg = cfg.min_segment.max(1);

        let fixed = match cfg.connections {
            Connections::Fixed(n) => Some(n.clamp(1, MAX_CONNECTIONS)),
            Connections::Auto => None,
        };
        let ceiling_cfg = cfg.max_connections.clamp(1, MAX_CONNECTIONS);
        let initial = fixed.unwrap_or(AUTO_START_CONNECTIONS.min(ceiling_cfg));

        // ---- Work out what we are downloading and what is already on disk.
        let (table, mut first_resp, size, ranged, etag, last_modified, validator, url, resumed) = match start {
            Start::New => unreachable!("probe happens before run_once"),
            Start::Fresh(probe) => {
                let info = probe.info;
                let ranged = info.resumable && info.size.is_some() && !force_single;
                let table = match info.size {
                    Some(size) if ranged => SegmentTable::even(size, initial, min_seg),
                    Some(size) => SegmentTable::single(size),
                    None => SegmentTable::single(UNKNOWN_END),
                };
                let validator = info.validator();
                let url = info.final_url.clone();
                let (size, etag, lm) = (info.size, info.etag.clone(), info.last_modified.clone());
                *self.progress.info.lock() = Some(info);
                (table, probe.response, size, ranged, etag, lm, validator, url, false)
            }
            Start::Resume(st) => {
                let on_disk = std::fs::metadata(&part).map(|m| m.len()).ok();
                if !st.is_consistent() || on_disk != Some(st.size) {
                    return Once::Restart { single: false, reason: "partial data is missing or damaged".into() };
                }
                let validator = st.validator();
                let table = SegmentTable::from_records(&st.segments);
                (table, None, Some(st.size), true, st.etag, st.last_modified, validator, st.url.clone(), true)
            }
        };
        let table = Arc::new(table);

        self.progress.size.store(size.unwrap_or(UNKNOWN_END), Ordering::Relaxed);
        self.progress.resumable.store(ranged, Ordering::Relaxed);
        self.progress.downloaded.store(table.fetched(), Ordering::Relaxed);
        *self.progress.table.lock() = Some(table.clone());

        let file = match open_part_file(&part, if ranged { size } else { None }) {
            Ok(f) => f,
            Err(e) => return Once::Done(Outcome::Failed(format!("cannot create {}: {e}", part.display()))),
        };
        if !ranged {
            // Nothing on disk is reusable without ranges.
            let _ = file.set_len(0);
        }
        let writer = Writer::spawn(file, table.clone());

        let (run_cancel, run_cancel_rx) = watch::channel(false);
        let run = Arc::new(Run {
            client: self.client.clone(),
            cfg: cfg.clone(),
            progress: self.progress.clone(),
            table: table.clone(),
            writer: writer.handle(),
            cancel: run_cancel_rx,
            size,
            ranged,
            range_proven: AtomicBool::new(resumed),
            validator,
            url: Mutex::new(url),
            streaming: AtomicU32::new(0),
            live: AtomicU32::new(0),
            target: AtomicU32::new(0),
            busy: Mutex::new(Busy { quiet_until: Instant::now(), last_reject: None, streak: 0 }),
            pressure: AtomicBool::new(false),
            gate: tokio::sync::Mutex::new(()),
            probers: AtomicU32::new(0),
        });

        let template = ResumeState {
            url: cfg.spec.url.clone(),
            size: size.unwrap_or(0),
            etag,
            last_modified: last_modified.clone(),
            segments: Vec::new(),
        };
        let make_state = move |segments| ResumeState { segments, ..template.clone() };

        // Periodic durable checkpoint, off the supervisor's back.
        let saver = ranged.then(|| {
            let (writer, path, make) = (writer.handle(), state_file.clone(), make_state.clone());
            AbortOnDrop(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(CHECKPOINT_INTERVAL).await;
                    let Ok(records) = writer.checkpoint(true).await else { return };
                    let state = make(records);
                    let p = path.clone();
                    let _ = tokio::task::spawn_blocking(move || save_json(&p, &state)).await;
                }
            }))
        });

        // ---- Supervisor loop.
        let mut workers: JoinSet<Result<(), WorkerError>> = JoinSet::new();
        let mut target = if ranged { initial } else { 1 };
        let mut ceiling = if ranged { fixed.unwrap_or(ceiling_cfg) } else { 1 };
        let mut auto_done = fixed.is_some() || !ranged;
        let mut baseline: Option<f64> = None;
        let mut proven_target = target;

        let mut paused = false;
        let mut fatal: Option<String> = None;
        let mut restart: Option<(bool, String)> = None;
        let mut fail_streak = 0u32;
        let mut backoff_until = Instant::now();
        let mut fail_mark = table.fetched();

        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_sample = (Instant::now(), self.progress.downloaded());
        let mut ema_speed = 0f64;
        let mut settle_from = Instant::now();
        let mut measuring: Option<(Instant, u64)> = None;
        let mut last_evict_check = Instant::now();

        loop {
            let stopping = paused || fatal.is_some() || restart.is_some();
            run.target.store(target, Ordering::Relaxed);
            if !stopping && Instant::now() >= backoff_until {
                while workers.len() < target as usize {
                    if let Some(resp) = first_resp.take() {
                        if let Some(seg) = table.claim_head() {
                            workers.spawn(worker(run.clone(), Some(seg), Some(resp), false));
                            continue;
                        }
                    }
                    if run.pressure.load(Ordering::Relaxed) {
                        // The server is rationing requests: one attempt at a time, and a
                        // segment is claimed only once it is that connection's turn.
                        if run.probers.load(Ordering::Relaxed) == 0 && table.has_work(min_seg, ranged) {
                            run.probers.fetch_add(1, Ordering::Relaxed);
                            workers.spawn(worker(run.clone(), None, None, true));
                        }
                        break;
                    }
                    match table.acquire(min_seg, ranged) {
                        Some(seg) => {
                            workers.spawn(worker(run.clone(), Some(seg), None, false));
                        }
                        None => break,
                    }
                }
            }
            self.progress.connections.store(workers.len() as u32, Ordering::Relaxed);

            if workers.is_empty()
                && (stopping || table.is_complete()) {
                    break;
                }
                // Otherwise we are waiting out a back-off; fall through to the tick.

            tokio::select! {
                _ = tick.tick() => {
                    let now = Instant::now();
                    let done = self.progress.downloaded();

                    let dt = now.duration_since(last_sample.0).as_secs_f64();
                    if dt >= 0.2 {
                        let inst = done.saturating_sub(last_sample.1) as f64 / dt;
                        ema_speed = if ema_speed == 0.0 { inst } else { ema_speed * 0.7 + inst * 0.3 };
                        self.progress.speed.store(ema_speed as u64, Ordering::Relaxed);
                        last_sample = (now, done);
                    }

                    if fail_streak > 0 && done >= fail_mark + 256 * 1024 {
                        fail_streak = 0;
                        *self.progress.last_error.lock() = None;
                    }

                    if now.duration_since(last_evict_check) >= Duration::from_secs(1) {
                        last_evict_check = now;
                        evict_slow(&table);
                    }
                }
                Some(joined) = workers.join_next(), if !workers.is_empty() => {
                    let res = joined.unwrap_or_else(|e| Err(WorkerError::Retry(format!("connection task failed: {e}"))));
                    log::debug!(
                        "worker ended: {res:?}; {} left, target {target}, streaming {}",
                        workers.len(),
                        run.streaming.load(Ordering::Relaxed)
                    );
                    match res {
                        Ok(()) | Err(WorkerError::Cancelled) | Err(WorkerError::Evicted) => {}
                        Err(WorkerError::Disk(msg)) => {
                            fatal.get_or_insert(format!("disk error: {msg}"));
                            let _ = run_cancel.send(true);
                        }
                        Err(WorkerError::RangeIgnored) => {
                            restart.get_or_insert((true, "server does not support resuming".into()));
                            let _ = run_cancel.send(true);
                        }
                        Err(WorkerError::Changed) => {
                            restart.get_or_insert((false, "the file changed on the server".into()));
                            let _ = run_cancel.send(true);
                        }
                        Err(WorkerError::Busy { code, streak }) => {
                            *run.url.lock() = cfg.spec.url.clone();
                            auto_done = true;
                            if run.streaming.load(Ordering::Relaxed) > 0 {
                                // Others are being served, so this is a per-client limit. The first
                                // burst of rejections tells us how many connections the server
                                // tolerates. Later rejections (rate limits, slots not yet freed)
                                // only delay requests — see `Run::note_busy` — and must not keep
                                // shrinking the ceiling, or one strict server collapses us to 1.
                                if streak <= 1 {
                                    ceiling = ceiling.min((workers.len() as u32).max(1));
                                    target = target.min(ceiling);
                                    proven_target = proven_target.min(ceiling);
                                }
                            } else if retryable_status(code) {
                                self.note_failure(&mut fail_streak, &mut backoff_until, &mut fail_mark, &mut fatal,
                                    format!("server returned HTTP {code}"), &run_cancel);
                            } else {
                                fatal.get_or_insert(format!("server returned HTTP {code}"));
                                let _ = run_cancel.send(true);
                            }
                        }
                        Err(WorkerError::Status(code)) => {
                            // A stale redirect target is the usual cause; go back through the original URL.
                            *run.url.lock() = cfg.spec.url.clone();
                            if retryable_status(code) {
                                self.note_failure(&mut fail_streak, &mut backoff_until, &mut fail_mark, &mut fatal,
                                    format!("server returned HTTP {code}"), &run_cancel);
                            } else {
                                fatal.get_or_insert(format!("server returned HTTP {code}"));
                                let _ = run_cancel.send(true);
                            }
                        }
                        Err(WorkerError::Retry(msg)) => {
                            self.note_failure(&mut fail_streak, &mut backoff_until, &mut fail_mark, &mut fatal, msg, &run_cancel);
                        }
                    }
                }
                _ = cancelled(&mut self.cancel), if !paused => {
                    paused = true;
                    let _ = run_cancel.send(true);
                }
            }

            // Adaptive connection count. After every change the new connections
            // get a moment to ramp up, then throughput is measured. Double while
            // a step buys at least 8 % more; undo the last step when it did not.
            if !auto_done && !paused && fatal.is_none() && restart.is_none() {
                let now = Instant::now();
                let done = self.progress.downloaded();
                let limited = cfg.limiter.rate() > 0 || cfg.global_limiter.rate() > 0;
                if limited || workers.len() < target as usize {
                    // Not a clean measurement: capped by the user, or short of connections.
                    settle_from = now;
                    measuring = None;
                } else if let Some((t0, bytes0)) = measuring {
                    let secs = now.duration_since(t0).as_secs_f64();
                    if secs >= AUTO_MEASURE.as_secs_f64() {
                        let measured = done.saturating_sub(bytes0) as f64 / secs;
                        let improved = baseline.is_none_or(|b| measured >= b * 1.08);
                        if !improved {
                            // Workers above the target retire as they finish their segments.
                            target = proven_target;
                            auto_done = true;
                        } else {
                            proven_target = target;
                            let next = (target * 2).min(ceiling);
                            if next > target && table.has_work(min_seg, true) {
                                baseline = Some(measured);
                                target = next;
                                settle_from = now;
                                measuring = None;
                            } else {
                                auto_done = true;
                            }
                        }
                    }
                } else if now.duration_since(settle_from) >= AUTO_SETTLE {
                    measuring = Some((now, done));
                }
            }
        }

        // ---- Wind down: everything that was fetched must reach the disk first.
        if let Some(saver) = saver {
            saver.stop().await;
        }
        drop(run);
        self.progress.connections.store(0, Ordering::Relaxed);
        self.progress.speed.store(0, Ordering::Relaxed);
        let checkpoint = writer.checkpoint(true).await;
        let disk_error = writer.handle().error();
        writer.close().await;

        if let Some((single, reason)) = restart {
            return Once::Restart { single, reason };
        }
        let records = match checkpoint {
            Ok(r) => r,
            Err(e) => return Once::Done(Outcome::Failed(format!("disk error: {}", disk_error.unwrap_or(e.to_string())))),
        };

        if fatal.is_none() && !paused && table.is_complete() && table.is_fully_written() {
            if let Err(e) = finalize(&part, &cfg.final_path, &state_file, last_modified.as_deref(), cfg.set_file_time) {
                return Once::Done(Outcome::Failed(format!("cannot save {}: {e}", cfg.final_path.display())));
            }
            self.progress.downloaded.store(table.fetched(), Ordering::Relaxed);
            if size.is_none() {
                self.progress.size.store(table.fetched(), Ordering::Relaxed);
            }
            return Once::Done(Outcome::Complete);
        }

        if ranged {
            let state = make_state(records);
            self.progress.downloaded.store(state.downloaded(), Ordering::Relaxed);
            if let Err(e) = save_json(&state_file, &state) {
                log::error!("cannot save resume state {}: {e}", state_file.display());
            }
        } else {
            // Without range support the partial file is useless; do not leave it behind.
            let _ = std::fs::remove_file(&part);
            self.progress.downloaded.store(0, Ordering::Relaxed);
        }
        Once::Done(match fatal {
            Some(msg) => Outcome::Failed(msg),
            None => Outcome::Paused,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn note_failure(
        &self,
        fail_streak: &mut u32,
        backoff_until: &mut Instant,
        fail_mark: &mut u64,
        fatal: &mut Option<String>,
        msg: String,
        run_cancel: &watch::Sender<bool>,
    ) {
        let now = Instant::now();
        // Several connections dying from one outage count as one failed round.
        if now >= *backoff_until {
            *fail_streak += 1;
            *fail_mark = self.progress.downloaded();
            if *fail_streak > self.cfg.max_retries {
                fatal.get_or_insert(msg.clone());
                let _ = run_cancel.send(true);
            } else {
                *backoff_until = now + backoff(*fail_streak);
            }
        }
        *self.progress.last_error.lock() = Some(msg);
    }
}

/// Probe with the same retry policy as the transfer itself: transient
/// failures back off and retry, definite ones (404, bad URL) fail at once.
/// `Err(Outcome::Paused)` means `cancel` fired.
pub async fn probe_with_retry(
    client: &Client,
    spec: &RequestSpec,
    max_retries: u32,
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
    on_retry: impl Fn(String),
) -> Result<Probe, Outcome> {
    let mut attempt = 0u32;
    loop {
        let res = tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(Outcome::Paused),
            r = tokio::time::timeout(timeout, http::probe(client, spec)) => r,
        };
        let err = match res {
            Ok(Ok(p)) => return Ok(p),
            Ok(Err(e @ (Error::InvalidUrl(_) | Error::Other(_) | Error::Io(_)))) => return Err(Outcome::Failed(e.to_string())),
            Ok(Err(Error::Status(code))) if !retryable_status(code) => {
                return Err(Outcome::Failed(Error::Status(code).to_string()))
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => "timed out waiting for the server".to_string(),
        };
        attempt += 1;
        if attempt > max_retries {
            return Err(Outcome::Failed(err));
        }
        on_retry(err);
        tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(Outcome::Paused),
            _ = tokio::time::sleep(backoff(attempt)) => {}
        }
    }
}

fn retryable_status(code: u16) -> bool {
    matches!(code, 408 | 425 | 429) || (500..600).contains(&code)
}

/// Kick connections that have crawled far below their peers for 10 s while
/// still holding real work, so the segment is retried on a fresh connection.
fn evict_slow(table: &SegmentTable) {
    let settled: Vec<Arc<Segment>> =
        table.owned_segments().into_iter().filter(|s| s.owned_for() > Duration::from_secs(5)).collect();
    if settled.len() < 3 {
        return;
    }
    let mut speeds: Vec<u64> = settled.iter().map(|s| s.speed()).collect();
    speeds.sort_unstable();
    let median = speeds[speeds.len() / 2];
    if median == 0 {
        return;
    }
    for seg in settled {
        if seg.speed().saturating_mul(5) < median && seg.remaining() > 4 * 1024 * 1024 {
            if seg.slow_secs.fetch_add(1, Ordering::Relaxed) + 1 >= 10 {
                seg.slow_secs.store(0, Ordering::Relaxed);
                seg.kick.notify_waiters();
            }
        } else {
            seg.slow_secs.store(0, Ordering::Relaxed);
        }
    }
}

fn finalize(part: &Path, final_path: &Path, state_file: &Path, last_modified: Option<&str>, set_time: bool) -> std::io::Result<()> {
    std::fs::rename(part, final_path)?;
    let _ = std::fs::remove_file(state_file);
    if set_time {
        if let Some(when) = last_modified.and_then(|v| chrono::DateTime::parse_from_rfc2822(v).ok()) {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(final_path) {
                let _ = f.set_modified(when.into());
            }
        }
    }
    Ok(())
}

/// One connection. Finishes a segment, then asks the table for more work.
async fn worker(
    run: Arc<Run>,
    assigned: Option<Arc<Segment>>,
    first: Option<Response>,
    counted_prober: bool,
) -> Result<(), WorkerError> {
    run.live.fetch_add(1, Ordering::Relaxed);
    let res = worker_loop(&run, assigned, first, counted_prober).await;
    run.live.fetch_sub(1, Ordering::Relaxed);
    res
}

async fn worker_loop(
    run: &Run,
    mut assigned: Option<Arc<Segment>>,
    mut first: Option<Response>,
    mut counted_prober: bool,
) -> Result<(), WorkerError> {
    loop {
        let job = next_job(run, assigned.take(), first.take(), std::mem::take(&mut counted_prober)).await?;
        let Some((seg, resp, body_end)) = job else { return Ok(()) };

        run.streaming.fetch_add(1, Ordering::Relaxed);
        let res = pump(run, &seg, resp, body_end).await;
        run.streaming.fetch_sub(1, Ordering::Relaxed);
        seg.release();
        res?;

        // The supervisor lowered the connection count: retire instead of taking more work.
        if run.live.load(Ordering::Relaxed) > run.target.load(Ordering::Relaxed) {
            return Ok(());
        }
    }
}

/// Find a segment and get a response for it. `Ok(None)`: nothing left to do.
/// The third element is the absolute offset at which the response body ends.
async fn next_job(
    run: &Run,
    assigned: Option<Arc<Segment>>,
    ready: Option<Response>,
    counted_prober: bool,
) -> Result<Option<(Arc<Segment>, Response, Option<u64>)>, WorkerError> {
    if let Some(resp) = ready {
        // The probe response: it asked for `bytes=0-`, so it runs to the end of the file.
        let seg = assigned.expect("the probe response comes with the head segment");
        return Ok(Some((seg, resp, run.size.filter(|_| run.ranged))));
    }

    let pressure = run.pressure.load(Ordering::Relaxed);
    let _prober = if counted_prober {
        Some(ProberGuard(&run.probers))
    } else if pressure {
        run.probers.fetch_add(1, Ordering::Relaxed);
        Some(ProberGuard(&run.probers))
    } else {
        None
    };
    let give_back = |seg: &Option<Arc<Segment>>| {
        if let Some(s) = seg {
            s.release();
        }
    };

    // After a rejection, requests are admitted one at a time and only once the
    // server's quiet period is over, instead of every idle connection retrying at once.
    let mut cancel = run.cancel.clone();
    let _turn = if pressure {
        tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => {
                give_back(&assigned);
                return Err(WorkerError::Cancelled);
            }
            guard = run.gate.lock() => Some(guard),
        }
    } else {
        None
    };
    loop {
        let until = run.busy.lock().quiet_until;
        if until <= Instant::now() {
            break;
        }
        tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => {
                give_back(&assigned);
                return Err(WorkerError::Cancelled);
            }
            _ = tokio::time::sleep_until(until.into()) => {}
        }
    }

    let seg = match assigned {
        Some(seg) => seg,
        None => match run.table.acquire(run.cfg.min_segment.max(1), run.ranged) {
            Some(seg) => seg,
            None => return Ok(None),
        },
    };
    let (pos, end) = seg.pos_end();
    if pos >= end {
        seg.release();
        return Ok(None);
    }
    // Normally a request covers exactly its segment, so the connection is
    // clean and reusable afterwards. Under pressure every accepted request is
    // precious: let it run on through whatever untouched segments follow.
    let req_end = if run.ranged && run.pressure.load(Ordering::Relaxed) { run.table.chain_end(&seg) } else { end };
    match open(run, pos, req_end).await {
        Ok(resp) => Ok(Some((seg, resp, run.ranged.then_some(req_end)))),
        Err(e) => {
            seg.release();
            Err(e)
        }
    }
}

async fn open(run: &Run, pos: u64, end: u64) -> Result<Response, WorkerError> {
    if !run.ranged && pos > 0 {
        // Cannot continue mid-file without ranges; the supervisor starts over.
        return Err(WorkerError::RangeIgnored);
    }
    let url = run.url.lock().clone();
    let mut rb = http::request(&run.client, &run.cfg.spec, &url);
    if run.ranged {
        rb = rb.header(header::RANGE, format!("bytes={}-{}", pos, end - 1));
        if let Some(v) = &run.validator {
            rb = rb.header(header::IF_RANGE, v);
        }
    }

    let mut cancel = run.cancel.clone();
    let sent_at = Instant::now();
    let sent = tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => return Err(WorkerError::Cancelled),
        r = tokio::time::timeout(run.cfg.stall_timeout, rb.send()) => r,
    };
    let resp = match sent {
        Err(_) => return Err(WorkerError::Retry("timed out waiting for the server".into())),
        Ok(Err(e)) => return Err(WorkerError::Retry(describe(&e))),
        Ok(Ok(r)) => r,
    };

    let status = resp.status();
    log::debug!("GET bytes={pos}-{} -> {status}", end.wrapping_sub(1));
    if !status.is_success() && status != StatusCode::RANGE_NOT_SATISFIABLE {
        let code = status.as_u16();
        // 429/503 are explicit push-back. Any other refusal while the same
        // server is happily serving our other connections is treated the same way.
        if matches!(code, 429 | 503) || run.streaming.load(Ordering::Relaxed) > 0 {
            let streak = run.note_busy(sent_at, retry_after(resp.headers()));
            return Err(WorkerError::Busy { code, streak });
        }
        return Err(WorkerError::Status(code));
    }
    if !run.ranged {
        return if status.is_success() {
            run.note_accepted();
            Ok(resp)
        } else {
            Err(WorkerError::Status(status.as_u16()))
        };
    }
    if status.is_success() {
        run.note_accepted();
    }
    match status {
        StatusCode::PARTIAL_CONTENT => {
            let range = resp
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range);
            match range {
                Some((start, _, total)) if start == pos => {
                    if total.is_some() && total != run.size {
                        return Err(WorkerError::Changed);
                    }
                    run.range_proven.store(true, Ordering::Relaxed);
                    *run.url.lock() = resp.url().to_string();
                    Ok(resp)
                }
                _ => Err(WorkerError::RangeIgnored),
            }
        }
        StatusCode::OK => {
            // The whole file came back. With `If-Range` on a server known to
            // do ranges that means the validator no longer matches.
            if run.range_proven.load(Ordering::Relaxed) {
                Err(WorkerError::Changed)
            } else {
                Err(WorkerError::RangeIgnored)
            }
        }
        StatusCode::RANGE_NOT_SATISFIABLE => Err(WorkerError::Changed),
        s => Err(WorkerError::Status(s.as_u16())),
    }
}

/// Move bytes from the response into the segment until it is full.
/// `body_end` is the absolute offset where this response's body ends, if known.
#[allow(unused_assignments)]
async fn pump(run: &Run, seg: &Arc<Segment>, mut resp: Response, body_end: Option<u64>) -> Result<(), WorkerError> {
    let mut cancel = run.cancel.clone();
    let mut buf = BytesMut::with_capacity(WRITE_BUF);
    let mut buf_off = seg.pos();
    let mut last_flush = Instant::now();
    let mut win = (Instant::now(), 0u64);
    let mut ema = 0f64;

    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                let data = buf.split().freeze();
                let len = data.len() as u64;
                if let Err(e) = run.writer.write(seg, buf_off, data).await {
                    return Err(WorkerError::Disk(e.to_string()));
                }
                buf_off += len;
                buf.reserve(WRITE_BUF);
                last_flush = Instant::now();
            }
        };
    }

    // The segment is full but the response carries on: take over the next
    // segment too if nobody has touched it. Offsets stay contiguous, so the
    // write buffer needs no special handling.
    let absorb = || body_end.is_some_and(|limit| run.table.absorb_next(seg, limit));

    let result = 'outer: loop {
        let next = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => break Err(WorkerError::Cancelled),
            _ = seg.kick.notified() => break Err(WorkerError::Evicted),
            c = tokio::time::timeout(run.cfg.stall_timeout, resp.chunk()) => c,
        };
        let chunk = match next {
            Err(_) => break Err(WorkerError::Retry("connection stalled".into())),
            Ok(Err(e)) => break Err(WorkerError::Retry(describe(&e))),
            Ok(Ok(Some(c))) => c,
            Ok(Ok(None)) => {
                if seg.end() == UNKNOWN_END {
                    seg.finish_at_pos();
                    break Ok(());
                }
                break if seg.remaining() == 0 { Ok(()) } else { Err(WorkerError::Retry("connection closed early".into())) };
            }
        };

        // Under a speed limit, take the chunk in slices so progress stays
        // smooth and a pause never waits behind a long token sleep.
        let rate = match (run.cfg.limiter.rate(), run.cfg.global_limiter.rate()) {
            (0, g) => g,
            (l, 0) => l,
            (l, g) => l.min(g),
        };
        let slice = if rate == 0 { chunk.len() } else { ((rate / 8) as usize).clamp(4096, 256 * 1024) };
        let mut rest = &chunk[..];
        while !rest.is_empty() {
            let want = rest.len().min(slice);
            if rate > 0 {
                tokio::select! {
                    biased;
                    _ = cancelled(&mut cancel) => break 'outer Err(WorkerError::Cancelled),
                    _ = async {
                        run.cfg.limiter.acquire(want).await;
                        run.cfg.global_limiter.acquire(want).await;
                    } => {}
                }
            }
            let took = seg.advance(want);
            if took > 0 {
                buf.extend_from_slice(&rest[..took]);
                run.progress.downloaded.fetch_add(took as u64, Ordering::Relaxed);
                win.1 += took as u64;
            }
            rest = &rest[took..];
            if took < want && !absorb() {
                // What follows belongs to another connection (the tail was
                // stolen, or the next segment is taken). The surplus is
                // discarded along with this connection.
                break 'outer Ok(());
            }
            if buf.len() >= WRITE_BUF {
                flush!();
            }
        }

        if seg.remaining() == 0 && !absorb() {
            if body_end == Some(seg.end()) {
                // Let hyper see the end of the body so the connection goes back to the pool.
                let _ = tokio::time::timeout(Duration::from_millis(20), resp.chunk()).await;
            }
            break Ok(());
        }
        if !buf.is_empty() && last_flush.elapsed() >= FLUSH_INTERVAL {
            flush!();
        }
        let elapsed = win.0.elapsed();
        if elapsed >= Duration::from_millis(500) {
            let inst = win.1 as f64 / elapsed.as_secs_f64();
            ema = if ema == 0.0 { inst } else { ema * 0.5 + inst * 0.5 };
            seg.set_speed(ema.max(1.0) as u64);
            win = (Instant::now(), 0);
        }
    };

    // Whatever arrived is good data regardless of how the loop ended.
    flush!();
    result
}
