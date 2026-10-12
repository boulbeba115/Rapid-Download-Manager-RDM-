//! Shared fixture for manager-level tests: an isolated data directory and
//! download folder, with the machine's proxy taken out of the picture.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rdm_core::http::ProxyConfig;
use rdm_core::manager::Event;
use rdm_core::testserver::{ServerOptions, TestServer};
use rdm_core::{Id, Manager, Status};

pub const MIB: u64 = 1024 * 1024;

pub struct Fixture {
    pub root: PathBuf,
    pub manager: Manager,
}

impl Fixture {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join("rdm-tests").join(format!("mgr-{name}-{}-{}", std::process::id(), fastrand::u32(..)));
        std::fs::create_dir_all(&root).unwrap();
        let fx = Self { manager: Self::open(&root), root };
        let mut cfg = fx.manager.config();
        cfg.categories = rdm_core::category::defaults(&fx.root.join("downloads"));
        cfg.net.proxy = ProxyConfig::None;
        cfg.api_port = 0;
        fx.manager.set_config(cfg).unwrap();
        fx
    }

    pub fn open(root: &Path) -> Manager {
        Manager::new(root.join("data"), || {}).unwrap()
    }

    pub fn server(&self, opts: ServerOptions) -> TestServer {
        self.manager.runtime().block_on(TestServer::start(opts))
    }

    /// Collect events until `pred` matches one, or fail after `secs`.
    pub fn wait_event(&self, secs: u64, mut pred: impl FnMut(&Event) -> bool) -> Vec<Event> {
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

    /// Wait until the download reaches `status`, failing with its details after `secs`.
    pub fn wait_status(&self, id: Id, status: Status, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let v = self.manager.view(id).expect("download exists");
            if v.d.status == status {
                return;
            }
            assert!(Instant::now() < deadline, "still {:?} (wanted {status:?}): {:?}", v.d.status, v.d);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn wait_bytes(&self, id: Id, bytes: u64, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while self.manager.view(id).unwrap().d.downloaded < bytes {
            assert!(Instant::now() < deadline, "never reached {bytes} bytes: {:?}", self.manager.view(id).unwrap().d);
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
