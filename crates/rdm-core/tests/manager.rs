//! Manager-level tests: list, naming, persistence, queues, removal (SPEC.md §6–7).

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rdm_core::http::{ProxyConfig, RequestSpec};
use rdm_core::manager::{Event, PostAction, MAIN_QUEUE};
use rdm_core::task::Connections;
use rdm_core::testserver::{verify_file, ServerOptions, TestServer};
use rdm_core::{Manager, NewDownload, Status};

const MIB: u64 = 1024 * 1024;

struct Fixture {
    root: PathBuf,
    manager: Manager,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join("rdm-tests").join(format!("mgr-{name}-{}-{}", std::process::id(), fastrand::u32(..)));
        std::fs::create_dir_all(&root).unwrap();
        let fx = Self { manager: Self::open(&root), root };
        // Keep downloads inside the fixture and off the machine's proxy.
        let mut cfg = fx.manager.config();
        cfg.categories = rdm_core::category::defaults(&fx.root.join("downloads"));
        cfg.net.proxy = ProxyConfig::None;
        cfg.api_port = 0;
        fx.manager.set_config(cfg).unwrap();
        fx
    }

    fn open(root: &PathBuf) -> Manager {
        Manager::new(root.join("data"), || {}).unwrap()
    }

    fn server(&self, opts: ServerOptions) -> TestServer {
        self.manager.runtime().block_on(TestServer::start(opts))
    }

    /// Collect events until `pred` matches one, or fail after `secs`.
    fn wait_event(&self, secs: u64, mut pred: impl FnMut(&Event) -> bool) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut seen = Vec::new();
        loop {
            for e in self.manager.take_events() {
                let hit = pred(&e);
                seen.push(e);
                if hit {
                    return seen;
                }
            }
            assert!(Instant::now() < deadline, "timed out; events so far: {seen:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.manager.shutdown();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn names_places_downloads_and_persists() {
    let fx = Fixture::new("basic");
    let mut opts = ServerOptions::new(3 * MIB + 3);
    opts.file_name = Some("Holiday Video.mp4".into());
    let (seed, size) = (opts.seed, opts.size);
    let server = fx.server(opts);

    // Added by bare URL: the name is a guess until the server answers.
    let id = fx.manager.add(NewDownload::new(RequestSpec::new(server.url())));
    fx.wait_event(20, |e| matches!(e, Event::Finished { id: i, ok: true } if *i == id));

    let v = fx.manager.view(id).unwrap();
    assert_eq!(v.d.status, Status::Complete);
    assert_eq!(v.d.file_name, "Holiday Video.mp4");
    assert_eq!(v.d.category, "Video");
    assert_eq!(v.d.dir, fx.root.join("downloads").join("Video"));
    assert_eq!((v.d.size, v.d.downloaded), (Some(size), size));
    verify_file(&v.d.path(), seed, size).unwrap();

    // Same URL again: must not overwrite the first file.
    let id2 = fx.manager.add(NewDownload::new(RequestSpec::new(server.url())));
    fx.wait_event(20, |e| matches!(e, Event::Finished { id: i, ok: true } if *i == id2));
    let v2 = fx.manager.view(id2).unwrap();
    assert_eq!(v2.d.file_name, "Holiday Video (1).mp4");
    verify_file(&v2.d.path(), seed, size).unwrap();

    // The list survives a restart.
    fx.manager.save_now();
    let reopened = Fixture::open(&fx.root);
    let views = reopened.views();
    assert_eq!(views.len(), 2);
    assert!(views.iter().all(|v| v.d.status == Status::Complete));
    assert_eq!(reopened.config().net.proxy, ProxyConfig::None);
}

#[test]
fn pause_resume_and_restart_of_the_app() {
    let fx = Fixture::new("pause");
    let mut opts = ServerOptions::new(12 * MIB + 1);
    opts.per_conn_rate = 512 * 1024;
    let (seed, size) = (opts.seed, opts.size);
    let server = fx.server(opts);

    let mut new = NewDownload::new(RequestSpec::new(server.url()));
    new.file_name = Some("big.iso".into());
    new.connections = Some(Connections::Fixed(8));
    let id = fx.manager.add(new);

    let deadline = Instant::now() + Duration::from_secs(20);
    while fx.manager.view(id).unwrap().d.downloaded < 3 * MIB {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let running = fx.manager.view(id).unwrap();
    assert_eq!(running.d.status, Status::Running);
    assert!(running.speed > 0 && running.connections > 0);
    assert!(!fx.manager.segments(id).is_empty());

    // Closing the app pauses and saves; a new instance picks the download up where it was.
    fx.manager.shutdown();
    let reopened = Fixture::open(&fx.root);
    let v = reopened.view(id).unwrap();
    assert_eq!(v.d.status, Status::Paused);
    assert!(v.d.downloaded >= 3 * MIB, "progress lost: {}", v.d.downloaded);
    assert_eq!(v.d.category, "Compressed");

    let before = server.stats.bytes_sent.load(Ordering::Relaxed);
    reopened.start(id);
    let deadline = Instant::now() + Duration::from_secs(30);
    while reopened.view(id).unwrap().d.status != Status::Complete {
        assert!(Instant::now() < deadline, "{:?}", reopened.view(id).unwrap().d);
        std::thread::sleep(Duration::from_millis(20));
    }
    verify_file(&reopened.view(id).unwrap().d.path(), seed, size).unwrap();
    let refetched = server.stats.bytes_sent.load(Ordering::Relaxed) - before;
    assert!(refetched < size - 2 * MIB, "re-downloaded too much: {refetched}");
    reopened.shutdown();
}

#[test]
fn queue_runs_limited_number_at_a_time() {
    let fx = Fixture::new("queue");
    let mut opts = ServerOptions::new(2 * MIB);
    opts.per_conn_rate = 2 * MIB;
    let server = fx.server(opts);

    let mut q = fx.manager.queues().into_iter().find(|q| q.id == MAIN_QUEUE).unwrap();
    q.max_concurrent = 2;
    q.schedule.on_complete = PostAction::ExitApp;
    fx.manager.update_queue(q);

    let ids: Vec<_> = (0..5)
        .map(|i| {
            let mut new = NewDownload::new(RequestSpec::new(server.url()));
            new.file_name = Some(format!("part{i}.bin"));
            new.connections = Some(Connections::Fixed(1));
            new.queue = Some(MAIN_QUEUE);
            new.start = false;
            fx.manager.add(new)
        })
        .collect();
    assert!(fx.manager.views().iter().all(|v| v.d.status == Status::Queued));
    assert_eq!(fx.manager.active_count(), 0);

    fx.manager.start_queue(MAIN_QUEUE);
    let mut peak = 0;
    let mut finished = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !finished {
        peak = peak.max(fx.manager.active_count());
        for e in fx.manager.take_events() {
            if let Event::QueueFinished { queue, action } = e {
                assert_eq!((queue, action), (MAIN_QUEUE, PostAction::ExitApp));
                finished = true;
            }
        }
        assert!(Instant::now() < deadline, "queue never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(peak, 2, "queue must run exactly 2 at a time");
    for id in ids {
        assert_eq!(fx.manager.view(id).unwrap().d.status, Status::Complete);
    }
    assert!(!fx.manager.queues()[0].running);
}

#[test]
fn removing_a_running_download_cleans_up() {
    let fx = Fixture::new("remove");
    let mut opts = ServerOptions::new(8 * MIB);
    opts.per_conn_rate = 256 * 1024;
    let server = fx.server(opts);

    let mut new = NewDownload::new(RequestSpec::new(server.url()));
    new.file_name = Some("doomed.zip".into());
    let id = fx.manager.add(new);
    let path = fx.manager.view(id).unwrap().d.path();
    let part = rdm_core::filename::part_path(&path);

    let deadline = Instant::now() + Duration::from_secs(20);
    while !part.exists() || fx.manager.view(id).unwrap().d.downloaded < 256 * 1024 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    fx.manager.remove(id, true);
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.manager.view(id).is_some() {
        assert!(Instant::now() < deadline, "download was never removed");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(fx.manager.views().is_empty());
    assert!(!part.exists() && !rdm_core::filename::state_path(&path).exists() && !path.exists());
}

#[test]
fn failed_download_reports_the_reason() {
    let fx = Fixture::new("fail");
    let mut opts = ServerOptions::new(MIB);
    opts.force_status = Some(403);
    let server = fx.server(opts);
    let id = fx.manager.add(NewDownload::new(RequestSpec::new(server.url())));
    fx.wait_event(10, |e| matches!(e, Event::Finished { id: i, ok: false } if *i == id));
    let v = fx.manager.view(id).unwrap();
    assert_eq!(v.d.status, Status::Failed);
    assert_eq!(v.d.error.as_deref(), Some("server returned HTTP 403"));
}
