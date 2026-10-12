//! Dedicated disk thread. Network tasks hand over `(offset, bytes)` and never
//! block on the disk themselves; a full channel pushes back on the sockets.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::segment::{SegRecord, Segment, SegmentTable};

/// Buffers in flight between the network and the disk. With 512 KiB buffers
/// this caps a download's write backlog at 32 MiB.
const QUEUE_DEPTH: usize = 64;

enum Job {
    Write { seg: Arc<Segment>, offset: u64, data: Bytes },
    /// Everything queued before this is on disk (and fsynced if asked) when it answers.
    Checkpoint { fsync: bool, reply: oneshot::Sender<io::Result<Vec<SegRecord>>> },
}

#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<Job>,
    error: Arc<Mutex<Option<String>>>,
}

pub struct Writer {
    handle: WriterHandle,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    pub fn spawn(file: File, table: Arc<SegmentTable>) -> Self {
        let (tx, mut rx) = mpsc::channel::<Job>(QUEUE_DEPTH);
        let error = Arc::new(Mutex::new(None));
        let err_slot = error.clone();
        let thread = std::thread::Builder::new()
            .name("rdm-writer".into())
            .spawn(move || {
                while let Some(job) = rx.blocking_recv() {
                    match job {
                        Job::Write { seg, offset, data } => {
                            if let Err(e) = write_all_at(&file, &data, offset) {
                                *err_slot.lock() = Some(e.to_string());
                                // Closing the channel makes every sender fail fast.
                                return;
                            }
                            seg.set_written(offset + data.len() as u64);
                        }
                        Job::Checkpoint { fsync, reply } => {
                            let res = if fsync { file.sync_data() } else { Ok(()) };
                            let _ = reply.send(res.map(|_| table.records()));
                        }
                    }
                }
            })
            .expect("spawn writer thread");
        Self { handle: WriterHandle { tx, error }, thread: Some(thread) }
    }

    pub fn handle(&self) -> WriterHandle {
        self.handle.clone()
    }

    pub async fn checkpoint(&self, fsync: bool) -> io::Result<Vec<SegRecord>> {
        self.handle.checkpoint(fsync).await
    }

    /// Stop the thread and close the file. All `WriterHandle`s must be gone
    /// (workers finished) or this would wait on them.
    pub async fn close(mut self) {
        let thread = self.thread.take();
        drop(self);
        if let Some(t) = thread {
            let _ = tokio::task::spawn_blocking(move || t.join()).await;
        }
    }
}

impl WriterHandle {
    pub async fn write(&self, seg: &Arc<Segment>, offset: u64, data: Bytes) -> io::Result<()> {
        self.tx.send(Job::Write { seg: seg.clone(), offset, data }).await.map_err(|_| self.io_error())
    }

    /// Wait until every queued write is on disk and return the matching segment records.
    pub async fn checkpoint(&self, fsync: bool) -> io::Result<Vec<SegRecord>> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Job::Checkpoint { fsync, reply }).await.is_err() {
            return Err(self.io_error());
        }
        rx.await.unwrap_or_else(|_| Err(self.io_error()))
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }

    fn io_error(&self) -> io::Error {
        io::Error::other(self.error().unwrap_or_else(|| "disk writer stopped".into()))
    }
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

/// Open (or create) the in-progress file and give it its final size.
///
/// The file is marked sparse first. Without that, NTFS zero-fills everything
/// between the old end of valid data and the first write at a high offset —
/// on a multi-gigabyte file that stalls the last segment for seconds to minutes.
pub fn open_part_file(path: &Path, size: Option<u64>) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    if let Some(size) = size {
        if file.metadata()?.len() != size {
            mark_sparse(&file);
            file.set_len(size)?;
        }
    }
    Ok(file)
}

#[cfg(windows)]
fn mark_sparse(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let mut returned = 0u32;
    // Best effort: FAT/exFAT volumes refuse, and the download still works there.
    unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        );
    }
}

#[cfg(not(windows))]
fn mark_sparse(_file: &File) {}
