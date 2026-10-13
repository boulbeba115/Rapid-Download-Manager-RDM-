//! Loopback HTTP API: how second instances of the app and the browser's
//! native-messaging host (`rdm-host`) hand work to the running application.
//! Bound to 127.0.0.1 only.
//!
//! Nothing that runs inside a browser may drive this — not a web page, and
//! not some other extension. Three independent checks enforce that:
//!
//! * every request must carry the secret from the `api-token` file in the
//!   data directory, which only programs running as the same Windows user can
//!   read. The file is rewritten each time the application starts;
//! * a request with an `Origin` header (which browsers attach to anything a
//!   page or an extension sends cross-origin) is refused outright, and no
//!   CORS headers are ever sent;
//! * the `Host` header must name the loopback address, so a hostile domain
//!   that resolves to 127.0.0.1 (DNS rebinding) gets nowhere.
//!
//! The browser extension therefore never talks to this port. It uses native
//! messaging, where the browser itself guarantees that only the extension
//! listed in the host's manifest can reach `rdm-host`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::manager::{AddRequest, Event, WeakManager, MAIN_QUEUE};
use crate::security::{constant_time_eq, random_token, redact_url};

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 256 * 1024;
const TOKEN_HEADER: &str = "x-rdm-token";
const TOKEN_FILE: &str = "api-token";
/// Longest values accepted from the browser.
const MAX_URL: usize = 8 * 1024;
const MAX_COOKIES: usize = 32 * 1024;
const MAX_SHORT: usize = 1024;

// ---------------------------------------------------------------- Token

/// Create this run's secret and store it where the user's own programs can read it.
pub fn create_token(data_dir: &Path) -> std::io::Result<String> {
    let token = random_token()?;
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(data_dir.join(TOKEN_FILE), &token)?;
    Ok(token)
}

/// The secret of the instance that uses `data_dir`, if one has ever run.
pub fn read_token(data_dir: &Path) -> Option<String> {
    let token = std::fs::read_to_string(data_dir.join(TOKEN_FILE)).ok()?;
    let token = token.trim();
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())).then(|| token.to_string())
}

// ---------------------------------------------------------------- Server

pub async fn serve(listener: TcpListener, manager: WeakManager, token: String) {
    let token = Arc::new(token);
    loop {
        let Ok((stream, peer)) = listener.accept().await else { return };
        if !peer.ip().is_loopback() {
            continue;
        }
        let (manager, token) = (manager.clone(), token.clone());
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle(stream, manager, &token)).await;
        });
    }
}

struct Request {
    method: String,
    path: String,
    host: Option<String>,
    has_origin: bool,
    token: Option<String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
        let mut tmp = [0u8; 2048];
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let (method, path) = (first.next()?.to_string(), first.next()?.to_string());
    let (mut host, mut has_origin, mut token, mut length) = (None, false, None, 0usize);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "host" => host = Some(v.trim().to_ascii_lowercase()),
            "origin" => has_origin = true,
            "content-length" => length = v.trim().parse().ok()?,
            TOKEN_HEADER => token = Some(v.trim().to_string()),
            _ => {}
        }
    }
    if length > MAX_BODY {
        return None;
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let mut tmp = [0u8; 4096];
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
        }
    }
    body.truncate(length);
    Some(Request { method, path, host, has_origin, token, body })
}

/// `127.0.0.1`, `localhost` or `[::1]`, with or without a port.
fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

fn authorized(req: &Request, token: &str) -> bool {
    !req.has_origin
        && req.host.as_deref().is_some_and(is_loopback_host)
        && req.token.as_deref().is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes()))
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[derive(Deserialize)]
struct CliBody {
    args: Vec<String>,
}

async fn handle(mut stream: TcpStream, manager: WeakManager, token: &str) {
    let Some(req) = read_request(&mut stream).await else { return };
    if !authorized(&req, token) {
        return respond(&mut stream, "403 Forbidden", r#"{"ok":false,"error":"forbidden"}"#).await;
    }
    let Some(manager) = manager.upgrade() else { return };

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/ping") => {
            let body = format!(r#"{{"ok":true,"app":"rdm","version":"{}"}}"#, env!("CARGO_PKG_VERSION"));
            respond(&mut stream, "200 OK", &body).await
        }
        ("GET", "/config") => {
            let exts = manager.config().capture_extensions;
            let body = serde_json::json!({ "ok": true, "captureExtensions": exts }).to_string();
            respond(&mut stream, "200 OK", &body).await
        }
        // A download offered by the browser. It is only ever *offered*: the user
        // confirms it in the Download File Info dialog, and where it is saved is
        // never the caller's choice.
        ("POST", "/add") => match serde_json::from_slice::<AddRequest>(&req.body).ok().and_then(from_browser) {
            Some(add) => {
                log::info!("add request from the browser extension: {}", redact_url(&add.url));
                manager.push_event(Event::AddRequest(add));
                respond(&mut stream, "200 OK", r#"{"ok":true}"#).await
            }
            None => respond(&mut stream, "400 Bad Request", r#"{"ok":false,"error":"bad request"}"#).await,
        },
        // The command line of a second instance, i.e. of the user themselves.
        ("POST", "/cli") => match serde_json::from_slice::<CliBody>(&req.body) {
            Ok(cli) => {
                for event in parse_cli(&cli.args) {
                    manager.push_event(event);
                }
                respond(&mut stream, "200 OK", r#"{"ok":true}"#).await
            }
            Err(_) => respond(&mut stream, "400 Bad Request", r#"{"ok":false}"#).await,
        },
        _ => respond(&mut stream, "404 Not Found", r#"{"ok":false,"error":"not found"}"#).await,
    }
}

fn is_http_url(url: &str) -> bool {
    url::Url::parse(url).map(|u| matches!(u.scheme(), "http" | "https")).unwrap_or(false)
}

/// Fit to be sent as an HTTP header value.
fn header_safe(value: &str, max: usize) -> bool {
    value.len() <= max && !value.chars().any(|c| c.is_control())
}

/// Reduce what the browser sent to what a browser may ask for. Everything
/// that decides *where* a file lands or *whether the user is asked* is
/// dropped, so the worst a misbehaving caller can do is open a dialog.
fn from_browser(req: AddRequest) -> Option<AddRequest> {
    if req.url.len() > MAX_URL || !is_http_url(&req.url) {
        return None;
    }
    Some(AddRequest {
        url: req.url,
        referer: req.referer.filter(|r| r.len() <= MAX_URL && is_http_url(r)),
        cookies: req.cookies.filter(|c| !c.is_empty() && header_safe(c, MAX_COOKIES)),
        user_agent: req.user_agent.filter(|u| !u.is_empty() && header_safe(u, MAX_SHORT)),
        file_name: req.file_name.filter(|n| n.len() <= MAX_SHORT).map(|n| crate::filename::sanitize(&n)).filter(|n| !n.is_empty()),
        ..Default::default()
    })
}

/// IDM-compatible switches: `/d URL` `/p dir` `/f name` `/n` (silent)
/// `/a` (add to queue, do not start) `/s` (start the main queue). A bare URL
/// works too. No arguments means "show the window".
pub fn parse_cli(args: &[String]) -> Vec<Event> {
    let mut add = AddRequest::default();
    let mut events = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        // Accept both `/d` and `-d` / `--d` spellings.
        let switch = arg.strip_prefix('/').or_else(|| arg.strip_prefix("--")).or_else(|| arg.strip_prefix('-'));
        match switch.map(str::to_ascii_lowercase).as_deref() {
            Some("d") => add.url = it.next().cloned().unwrap_or_default(),
            Some("p") => add.dir = it.next().map(PathBuf::from),
            Some("f") => add.file_name = it.next().cloned(),
            Some("n") => add.silent = true,
            Some("a") => {
                add.silent = true;
                add.queue_only = true;
            }
            Some("s") => events.push(Event::StartQueue(MAIN_QUEUE)),
            Some("q") => add.quit_when_done = true,
            // Ask the running instance to exit (used by the installer before replacing files).
            Some("quit") => events.push(Event::Quit),
            // Open the "set up the browser extension" guide (offered on the installer's last page).
            Some("browser-setup") => events.push(Event::BrowserSetup),
            // `/h` (hang up the modem) has no equivalent.
            Some(_) => {}
            None if is_http_url(arg) && add.url.is_empty() => add.url = arg.clone(),
            None => {}
        }
    }
    if is_http_url(&add.url) {
        events.insert(0, Event::AddRequest(add));
    }
    if events.is_empty() {
        events.push(Event::ShowWindow);
    }
    events
}

// ---------------------------------------------------------------- Client

pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        self.status == 200
    }
}

/// One request to the instance listening on `port`. Fails quickly when nothing is there.
pub fn call(port: u16, token: &str, method: &str, path: &str, body: Option<&str>) -> std::io::Result<Reply> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-RDM-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes())?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    let bad = || std::io::Error::other("malformed reply");
    let status = resp.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).ok_or_else(bad)?;
    Ok(Reply { status, body })
}

/// Hand our command line to the instance that uses `data_dir`, if it is running.
/// Returns false if nothing that accepts our token answers on the port.
pub fn forward_to_running(port: u16, data_dir: &Path, args: &[String]) -> bool {
    let Some(token) = read_token(data_dir) else { return false };
    let body = serde_json::json!({ "args": args }).to_string();
    call(port, &token, "POST", "/cli", Some(&body)).is_ok_and(|r| r.ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_switches() {
        let ev = parse_cli(&args(&["/d", "https://h.com/a.zip", "/p", "D:\\dl", "/f", "b.zip", "/n", "/s"]));
        assert_eq!(ev.len(), 2);
        match &ev[0] {
            Event::AddRequest(a) => {
                assert_eq!(a.url, "https://h.com/a.zip");
                assert_eq!(a.dir.as_deref(), Some(std::path::Path::new("D:\\dl")));
                assert_eq!(a.file_name.as_deref(), Some("b.zip"));
                assert!(a.silent && !a.queue_only);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(ev[1], Event::StartQueue(MAIN_QUEUE)));

        assert!(matches!(parse_cli(&args(&["https://h.com/x"]))[0], Event::AddRequest(_)));
        assert!(matches!(parse_cli(&args(&[]))[0], Event::ShowWindow));
        // Not a web URL: never turned into a download.
        assert!(matches!(parse_cli(&args(&["/d", "file:///c:/secret"]))[0], Event::ShowWindow));
        match &parse_cli(&args(&["/a", "/d", "http://h/x"]))[0] {
            Event::AddRequest(a) => assert!(a.silent && a.queue_only),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_cli(&args(&["--browser-setup"]))[0], Event::BrowserSetup));
    }

    #[test]
    fn loopback_hosts() {
        for ok in ["127.0.0.1", "127.0.0.1:47821", "localhost:47821", "[::1]:47821"] {
            assert!(is_loopback_host(ok), "{ok}");
        }
        for bad in ["evil.example", "evil.example:47821", "127.0.0.1.evil.example", "", "[::2]:1"] {
            assert!(!is_loopback_host(bad), "{bad}");
        }
    }

    #[test]
    fn browser_requests_cannot_choose_where_or_whether() {
        let raw = r#"{
            "url": "https://h.com/a.zip", "referer": "https://h.com/page", "cookies": "sid=1; x=2",
            "userAgent": "Mozilla/5.0", "fileName": "..\\..\\Startup\\evil.exe",
            "dir": "C:\\Users\\me\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Startup",
            "silent": true, "queueOnly": true, "quitWhenDone": true
        }"#;
        let add = from_browser(serde_json::from_str(raw).unwrap()).unwrap();
        assert_eq!(add.url, "https://h.com/a.zip");
        assert_eq!(add.cookies.as_deref(), Some("sid=1; x=2"));
        assert_eq!(add.file_name.as_deref(), Some("evil.exe"));
        assert_eq!(add.dir, None);
        assert!(!add.silent && !add.queue_only && !add.quit_when_done);

        let bad = |json: &str| from_browser(serde_json::from_str(json).unwrap());
        assert!(bad(r#"{"url":"file:///C:/Windows/win.ini"}"#).is_none());
        assert!(bad(r#"{"url":"javascript:alert(1)"}"#).is_none());
        // Header injection through a cookie or User-Agent is dropped, not forwarded.
        let add = bad(r#"{"url":"http://h/x","cookies":"a=b\r\nX-Evil: 1","userAgent":"UA\nHost: evil","referer":"file:///x"}"#).unwrap();
        assert_eq!((add.cookies, add.user_agent, add.referer), (None, None, None));
    }

    /// The whole point of the token: without it nothing is served, with it the
    /// request still has to look like it came from a local program.
    #[test]
    fn only_token_holders_get_in() {
        let dir = std::env::temp_dir().join(format!("rdm-api-test-{}", std::process::id()));
        let manager = crate::Manager::new(dir.clone(), || {}).unwrap();
        let mut cfg = manager.config();
        cfg.api_port = 0;
        manager.set_config(cfg).unwrap();
        let port = manager.start_api().unwrap();
        let token = read_token(&dir).expect("the token file is written when the API starts");

        let raw = |head: &str| -> String {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(format!("GET /config HTTP/1.1\r\n{head}Connection: close\r\n\r\n").as_bytes()).unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            out
        };
        let with = |extra: &str| raw(&format!("Host: 127.0.0.1:{port}\r\nX-RDM-Token: {token}\r\n{extra}"));

        assert!(with("").starts_with("HTTP/1.1 200"));
        // No token, a wrong token, or the old "I am a native client" header: refused.
        assert!(raw("Host: 127.0.0.1\r\n").starts_with("HTTP/1.1 403"));
        assert!(raw(&format!("Host: 127.0.0.1\r\nX-RDM-Token: {}\r\n", "0".repeat(64))).starts_with("HTTP/1.1 403"));
        assert!(raw("Host: 127.0.0.1\r\nX-RDM-Client: cli\r\n").starts_with("HTTP/1.1 403"));
        // Anything a browser sends is refused even with the right token.
        let page = with("Origin: https://evil.example\r\n");
        assert!(page.starts_with("HTTP/1.1 403") && !page.to_ascii_lowercase().contains("access-control"), "{page}");
        assert!(with("Origin: chrome-extension://abcdefghijklmnopabcdefghijklmnop\r\n").starts_with("HTTP/1.1 403"));
        // DNS rebinding: right token, wrong Host.
        assert!(raw(&format!("Host: evil.example:{port}\r\nX-RDM-Token: {token}\r\n")).starts_with("HTTP/1.1 403"));

        // The client helper speaks the same protocol.
        assert!(call(port, &token, "GET", "/ping", None).unwrap().body.contains(r#""app":"rdm""#));
        assert_eq!(call(port, "wrong", "GET", "/ping", None).unwrap().status, 403);
        assert!(forward_to_running(port, &dir, &args(&[])));
        manager.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
