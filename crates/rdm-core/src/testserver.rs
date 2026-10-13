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
    /// Demand these HTTP Basic credentials (`401` otherwise).
    pub require_auth: Option<(String, String)>,
    /// The first N connections accepted are throttled to `slow_rate` bytes/s
    /// (a congested route / bad CDN node).
    pub slow_connections: u64,
    pub slow_rate: u64,
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
            require_auth: None,
            slow_connections: 0,
            slow_rate: 0,
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
                let conn_index = s.connections.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(serve_connection(stream, o.clone(), s.clone(), d.clone(), conn_index));
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

/// One video (number 4242424242, 136 s) with an HD and an SD file, and a second
/// one (5151515151, 137 s): the keys and the self-describing `efg` parameter
/// (base64 JSON: encoding, video number, length) as Facebook's pages carry them.
const FB_DATA: &str = r#"{"data":{"video":{"id":"1","browser_native_hd_url":"https:\/\/video.test.fbcdn.net\/v\/hd-{VIA}.mp4?efg=eyJ2ZW5jb2RlX3RhZyI6Inhwdl9wcm9ncmVzc2l2ZS5GQUNFQk9PSy4uQzMuNzIwLmRhc2hfaDI2NC1iYXNpYy1nZW4yXzcyMHAiLCJ4cHZfYXNzZXRfaWQiOjQyNDI0MjQyNDIsImR1cmF0aW9uX3MiOjEzNn0&oh=1","browser_native_sd_url":"https:\/\/video.test.fbcdn.net\/v\/sd-{VIA}.mp4?efg=eyJ2ZW5jb2RlX3RhZyI6Inhwdl9wcm9ncmVzc2l2ZS5GQUNFQk9PSy4uQzMuMzYwLnN2ZV9zZCIsInhwdl9hc3NldF9pZCI6NDI0MjQyNDI0MiwiZHVyYXRpb25fcyI6MTM2fQ&oh=2"},"next":{"id":"2","browser_native_hd_url":null,"browser_native_sd_url":"https:\/\/video.test.fbcdn.net\/v\/other-{VIA}.mp4?efg=eyJ2ZW5jb2RlX3RhZyI6Inhwdl9wcm9ncmVzc2l2ZS5GQUNFQk9PSy4uQzMuMzYwLnN2ZV9zZCIsInhwdl9hc3NldF9pZCI6NTE1MTUxNTE1MSwiZHVyYXRpb25fcyI6MTM3fQ&oh=3"}}}"#;

/// The page behaves the way advert-laden video sites do, because that is what
/// the extension's button has to survive: it grabs every click anywhere on the
/// page (opening a new tab and hiding the click from everything else), and it
/// keeps an invisible layer on top of the player, above anything added later.
const PLAYER_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>RDM test player</title>
<body style="margin:0"><video id="v" src="movie.mp4" width="640" height="360" controls muted></video>
<script>
window.hijack = { seen: 0, opened: 0 };
for (const type of ["click", "mousedown", "mouseup", "pointerdown", "pointerup"]) {
  window.addEventListener(type, (event) => {
    if (event.target && event.target.id === "rdm-test-link") return;
    hijack.seen += 1;
    if (type === "click") { hijack.opened += 1; window.open("about:blank"); }
    event.stopImmediatePropagation();
  }, true);
}
const layer = document.createElement("div");
layer.id = "layer";
layer.style.cssText = "position:fixed;left:0;top:0;width:640px;height:360px;z-index:2147483647";
const keepOnTop = () => { if (document.documentElement.lastElementChild !== layer) document.documentElement.appendChild(layer); };
new MutationObserver(keepOnTop).observe(document.documentElement, { childList: true });
keepOnTop();
</script></body>"#;

struct Request {
    path: String,
    range: Option<(u64, Option<u64>)>,
    if_range: Option<String>,
    authorization: Option<String>,
}

async fn read_request(stream: &mut TcpStream, carry: &mut Vec<u8>) -> Option<Request> {
    loop {
        if let Some(end) = carry.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&carry[..end]).into_owned();
            carry.drain(..end + 4);
            let path = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").to_string();
            let mut req = Request { path, range: None, if_range: None, authorization: None };
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
                    "authorization" => req.authorization = Some(v.to_string()),
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

async fn serve_connection(mut stream: TcpStream, opts: Arc<Mutex<ServerOptions>>, stats: Arc<Stats>, drops_left: Arc<AtomicI64>, conn_index: u64) {
    let mut carry = Vec::new();
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        let Some(req) = read_request(&mut stream, &mut carry).await else { return };
        stats.requests.fetch_add(1, Ordering::Relaxed);
        let o = opts.lock().clone();
        if !o.latency.is_zero() {
            tokio::time::sleep(o.latency).await;
        }

        // A page with a video on it, for testing the browser extension's page button.
        // The video's address is `movie.mp4` on this server (random bytes, but the
        // extension only cares that a <video> element asked for it).
        if req.path.split('?').next().is_some_and(|p| p.ends_with("/player.html")) {
            let body = PLAYER_PAGE;
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n", body.len());
            if stream.write_all(head.as_bytes()).await.is_err() || stream.write_all(body.as_bytes()).await.is_err() {
                return;
            }
            continue;
        }

        // Page data in the form Facebook sends it, for testing how the extension
        // finds a video's whole-file addresses. `?via=x` makes the addresses distinct.
        if req.path.split('?').next().is_some_and(|p| p.ends_with("/fb-data.json")) {
            let via: String = req.path.split("via=").nth(1).unwrap_or("").chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            let body = FB_DATA.replace("{VIA}", &via);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len());
            if stream.write_all(head.as_bytes()).await.is_err() || stream.write_all(body.as_bytes()).await.is_err() {
                return;
            }
            continue;
        }

        if let Some((user, pass)) = &o.require_auth {
            use base64::Engine;
            let want = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")));
            if req.authorization.as_deref() != Some(want.as_str()) {
                stats.rejected.fetch_add(1, Ordering::Relaxed);
                let head = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n";
                if stream.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                continue;
            }
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
        let rate = if conn_index < o.slow_connections { o.slow_rate.max(1) } else { o.per_conn_rate };
        let step = if rate == 0 { chunk.len() } else { ((rate / 20) as usize).clamp(512, chunk.len()) };
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
            if rate > 0 {
                // Pace against the wall clock so sleep granularity does not accumulate.
                let due = started + Duration::from_secs_f64(sent as f64 / rate as f64);
                tokio::time::sleep_until(due).await;
            }
        }
        if !o.send_length {
            let _ = stream.shutdown().await;
            return;
        }
    }
}
