//! `rdm.log` in the data directory: what RDM was asked to do and what went wrong.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

const MAX_SIZE: u64 = 1024 * 1024;

struct FileLog(Mutex<File>);

impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info && m.target().starts_with("rdm")
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if let Ok(mut f) = self.0.lock() {
            let _ = writeln!(f, "{} {:<5} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), record.level(), record.args());
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.0.lock() {
            let _ = f.flush();
        }
    }
}

/// Start logging to `<data_dir>/rdm.log`. The previous log is kept as
/// `rdm.log.old` once it grows past 1 MB. Failure to open the file only means no log.
pub fn init(data_dir: &Path) {
    let _ = std::fs::create_dir_all(data_dir);
    let path = data_dir.join("rdm.log");
    if std::fs::metadata(&path).map(|m| m.len() > MAX_SIZE).unwrap_or(false) {
        let _ = std::fs::rename(&path, data_dir.join("rdm.log.old"));
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        if log::set_boxed_logger(Box::new(FileLog(Mutex::new(file)))).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
    }
}
