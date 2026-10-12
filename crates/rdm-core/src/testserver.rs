//! A small HTTP/1.1 file server that can misbehave on purpose. It serves a
//! deterministic pseudo-random file so any byte range can be generated and
//! verified without keeping the file in memory.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub size: u64,
    pub seed: u64,
    /// Honour `Range` (206). When false every request gets the whole file.
    pub ranges: bool,
    /// Send `Content-Length`. When false the body ends by closing the connection.
    pub send_length: bool,
    /// Bytes per second per connection; 0 = unthrottled.
    pub per_conn_rate: u64,
    /// Concurrent responses allowed; more get `503`. 0 = unlimited.
    pub max_concurrent: usize,
    /// Cut the connection after this many body bytes…
    pub drop_after: Option<u64>,
    /// …for this many responses (counted down), then behave.
    pub drop_count: i64,
    pub etag: Option<String>,
    pub file_name: Option<String>,
    /// Delay before the response headers (simulated round-trip time).
    pub latency: Duration,
    /// Answer every request with this status instead of the file.
    pub force_status: Option<u16>,
    /// Accept at most this many requests per window; the rest get `429` with
    /// `Retry-After`. Rejected requests extend nothing and count for nothing.
    pub request_limit: Option<(usize, Duration)>,
}

impl ServerOptions {
    pub fn new(size: u64) -> Self {
        Self {
            size,
            seed: 0x5eed,
            ranges: true,
            send_length: true,
            per_conn_rate: 0,
            max_concurrent: 0,
            drop_after: None,
            drop_count: 0,
            etag: Some("\"v1\"".into()),
            file_name: None,
            latency: Duration::ZERO,
            force_status: None,
            request_limit: None,
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub requests: AtomicU64,
    pub connections: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub rejected: AtomicU64,
    pub concurrent: AtomicUsize,
    pub peak_concurrent: AtomicUsize,
    /// Arrival times of recently accepted requests, for `request_limit`.
    recent: Mutex<std::collections::VecDeque<std::time::Instant>>,
}

pub struct TestServer {
    pub addr: SocketAddr,
    opts: Arc<Mutex<ServerOptions>>,
    drops_left: Arc<AtomicI64>,
    pub stats: Arc<Stats>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl TestServer {
    pub async fn start(opts: ServerOptions) -> Self {
        Self::start_on(opts, 0).await
    }

    pub async fn start_on(opts: ServerOptions, port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind test server");
        let addr = listener.local_addr().unwrap();
        let drops_left = Arc::new(AtomicI64::new(opts.drop_count));
        let opts = Arc::new(Mutex::new(opts));
        let stats = Arc::new(Stats::default());
        let (o, s, d) = (opts.clone(), stats.clone(), drops_left.clone());
        let accept = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let _ = stream.set_nodelay(true);
                s.connections.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(serve_connection(stream, o.clone(), s.clone(), d.clone()));
            }
        });
        Self { addr, opts, drops_left, stats, accept }
    }

    pub fn url(&self) -> String {
        format!("http://{}/file.bin", self.addr)
    }

    /// Change behaviour while running (e.g. "the file changed on the server").
    pub fn update(&self, f: impl FnOnce(&mut ServerOptions)) {
        let mut o = self.opts.lock();
        f(&mut o);
        self.drops_left.store(o.drop_count, Ordering::Relaxed);
    }

    pub fn options(&self) -> ServerOptions {
        self.opts.lock().clone()
    }
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Fill `buf` with the file's content starting at `offset`.
pub fn fill(seed: u64, mut offset: u64, buf: &mut [u8]) {
    let mut i = 0;
    while i < buf.len() {
        let word = splitmix64(seed ^ (offset / 8)).to_le_bytes();
        let from = (offset % 8) as usize;
        let n = (8 - from).min(buf.len() - i);
        buf[i..i + n].copy_from_slice(&word[from..from + n]);
        i += n;
        offset += n as u64;
    }
}

/// Compare a file on disk with what the server would have served.
pub fn verify_file(path: &Path, seed: u64, size: u64) -> Result<(), String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    if len != size {
        return Err(format!("size mismatch: file is {len} bytes, expected {size}"));
    }
    let mut got = vec![0u8; 1 << 20];
    let mut want = vec![0u8; 1 << 20];
    let mut offset = 0u64;
    while offset < size {
        let n = ((size - offset) as usize).min(got.len());
        f.read_exact(&mut got[..n]).map_err(|e| e.to_string())?;
        fill(seed, offset, &mut want[..n]);
        if got[..n] != want[..n] {
            let at = got[..n].iter().zip(&want[..n]).position(|(a, b)| a != b).unwrap();
            return Err(format!("content mismatch at byte {}", offset + at as u64));
        }
        offset += n as u64;
    }
    Ok(())
}

struct Request {
    range: Option<(u64, Option<u64>)>,
    if_range: Option<String>,
}

async fn read_request(stream: &mut TcpStream, carry: &mut Vec<u8>) -> Option<Request> {
    loop {
        if let Some(end) = carry.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&carry[..end]).into_owned();
            carry.drain(..end + 4);
            let mut req = Request { range: None, if_range: None };
            for line in head.lines().skip(1) {
                let Some((k, v)) = line.split_once(':') else { continue };
                let v = v.trim();
                match k.trim().to_ascii_lowercase().as_str() {
                    "range" => {
                        if let Some((a, b)) = v.strip_prefix("bytes=").and_then(|r| r.split_once('-')) {
                            if let Ok(a) = a.trim().parse() {
                                req.range = Some((a, b.trim().parse().ok()));
                            }
                        }
                    }
                    "if-range" => req.if_range = Some(v.to_string()),
                    _ => {}
                }
            }
            return Some(req);
        }
        if carry.len() > 64 * 1024 {
            return None;
        }
        let mut tmp = [0u8; 4096];
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => carry.extend_from_slice(&tmp[..n]),
        }
    }
}

struct Slot(Arc<Stats>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.concurrent.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn serve_connection(mut stream: TcpStream, opts: Arc<Mutex<ServerOptions>>, stats: Arc<Stats>, drops_left: Arc<AtomicI64>) {
    let mut carry = Vec::new();
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        let Some(req) = read_request(&mut stream, &mut carry).await else { return };
        stats.requests.fetch_add(1, Ordering::Relaxed);
        let o = opts.lock().clone();
        if !o.latency.is_zero() {
            tokio::time::sleep(o.latency).await;
        }

        if let Some(code) = o.force_status {
            let head = format!("HTTP/1.1 {code} Forced\r\nContent-Length: 0\r\n\r\n");
            if stream.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            continue;
        }

        if let Some((max, window)) = o.request_limit {
            let over = {
                let mut recent = stats.recent.lock();
                let now = std::time::Instant::now();
                while recent.front().is_some_and(|t| now.duration_since(*t) > window) {
                    recent.pop_front();
                }
                if recent.len() >= max {
                    true
                } else {
                    recent.push_back(now);
                    false
                }
            };
            if over {
                stats.rejected.fetch_add(1, Ordering::Relaxed);
                let head = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: {}\r\nContent-Length: 0\r\n\r\n",
                    window.as_secs().max(1)
                );
                if stream.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                continue;
            }
        }

        let now = stats.concurrent.fetch_add(1, Ordering::Relaxed) + 1;
        let _slot = Slot(stats.clone());
        if o.max_concurrent > 0 && now > o.max_concurrent {
            stats.rejected.fetch_add(1, Ordering::Relaxed);
            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            return;
        }
        stats.peak_concurrent.fetch_max(now, Ordering::Relaxed);

        let validator_ok = match (&req.if_range, &o.etag) {
            (Some(sent), Some(tag)) => sent == tag,
            (Some(_), None) => false,
            (None, _) => true,
        };
        let range = if o.ranges && validator_ok { req.range } else { None };

        let (start, end, status) = match range {
            Some((a, _)) if a >= o.size && o.size > 0 => {
                let head = format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\n\r\n", o.size);
                if stream.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                continue;
            }
            Some((a, b)) if o.size > 0 => (a, b.map_or(o.size, |b| (b + 1).min(o.size)), 206),
            _ => (0, o.size, 200),
        };

        let mut head = format!("HTTP/1.1 {status} OK\r\nContent-Type: application/octet-stream\r\n");
        if o.ranges {
            head.push_str("Accept-Ranges: bytes\r\n");
        }
        if let Some(tag) = &o.etag {
            head.push_str(&format!("ETag: {tag}\r\n"));
        }
        if let Some(name) = &o.file_name {
            head.push_str(&format!("Content-Disposition: attachment; filename=\"{name}\"\r\n"));
        }
        if status == 206 {
            head.push_str(&format!("Content-Range: bytes {}-{}/{}\r\n", start, end - 1, o.size));
        }
        if o.send_length {
            head.push_str(&format!("Content-Length: {}\r\n", end - start));
        } else {
            head.push_str("Connection: close\r\n");
        }
        head.push_str("\r\n");
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }

        let cut = match o.drop_after {
            Some(n) if drops_left.fetch_sub(1, Ordering::Relaxed) > 0 => Some(n),
            _ => None,
        };
        let step = if o.per_conn_rate == 0 { chunk.len() } else { ((o.per_conn_rate / 20) as usize).clamp(512, chunk.len()) };
        let started = tokio::time::Instant::now();
        let mut sent = 0u64;
        let mut pos = start;
        while pos < end {
            let mut n = ((end - pos) as usize).min(step);
            if let Some(limit) = cut {
                if sent >= limit {
                    // Abrupt close mid-body.
                    return;
                }
                n = n.min((limit - sent) as usize);
            }
            fill(o.seed, pos, &mut chunk[..n]);
            if stream.write_all(&chunk[..n]).await.is_err() {
                return;
            }
            pos += n as u64;
            sent += n as u64;
            stats.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
            if o.per_conn_rate > 0 {
                // Pace against the wall clock so sleep granularity does not accumulate.
                let due = started + Duration::from_secs_f64(sent as f64 / o.per_conn_rate as f64);
                tokio::time::sleep_until(due).await;
            }
        }
        if !o.send_length {
            let _ = stream.shutdown().await;
            return;
        }
    }
}
