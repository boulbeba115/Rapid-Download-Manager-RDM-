//! Loopback HTTP API: how the browser extension and second instances of the
//! app hand work to the running one. Bound to 127.0.0.1 only.
//!
//! A web page must never be able to drive this. Browsers attach an `Origin`
//! header to every cross-origin POST, so requests are accepted only when
//! `Origin` is a browser-extension origin, or when there is no `Origin` at all
//! *and* the custom `X-RDM-Client` header is present (a native caller; a page
//! cannot send custom headers without a CORS pre-flight, which is refused).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::manager::{AddRequest, Event, WeakManager, MAIN_QUEUE};

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 256 * 1024;
const CLIENT_HEADER: &str = "x-rdm-client";

pub async fn serve(listener: TcpListener, manager: WeakManager) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else { return };
        if !peer.ip().is_loopback() {
            continue;
        }
        let manager = manager.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle(stream, manager)).await;
        });
    }
}

struct Request {
    method: String,
    path: String,
    origin: Option<String>,
    native: bool,
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
    let (mut origin, mut native, mut length) = (None, false, 0usize);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "origin" => origin = Some(v.trim().to_string()),
            "content-length" => length = v.trim().parse().ok()?,
            CLIENT_HEADER => native = true,
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
    Some(Request { method, path, origin, native, body })
}

fn is_extension_origin(origin: &str) -> bool {
    origin.starts_with("chrome-extension://") || origin.starts_with("moz-extension://")
}

async fn respond(stream: &mut TcpStream, status: &str, cors_origin: Option<&str>, body: &str) {
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    if let Some(origin) = cors_origin {
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: GET, POST\r\nAccess-Control-Allow-Headers: content-type\r\nVary: Origin\r\n"
        ));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[derive(Deserialize)]
struct CliBody {
    args: Vec<String>,
}

async fn handle(mut stream: TcpStream, manager: WeakManager) {
    let Some(req) = read_request(&mut stream).await else { return };
    let ext_origin = req.origin.as_deref().filter(|o| is_extension_origin(o));
    let from_extension = ext_origin.is_some();
    let from_native = req.origin.is_none() && req.native;
    let Some(manager) = manager.upgrade() else { return };

    match (req.method.as_str(), req.path.as_str()) {
        // Pre-flight: only extensions get an answer that lets the real request through.
        ("OPTIONS", _) if from_extension => respond(&mut stream, "204 No Content", ext_origin, "").await,
        ("GET", "/ping") => {
            let body = format!(r#"{{"app":"rdm","version":"{}"}}"#, env!("CARGO_PKG_VERSION"));
            respond(&mut stream, "200 OK", ext_origin, &body).await
        }
        ("GET", "/config") if from_extension || from_native => {
            let exts = manager.config().capture_extensions;
            let body = serde_json::json!({ "captureExtensions": exts }).to_string();
            respond(&mut stream, "200 OK", ext_origin, &body).await
        }
        ("POST", "/add") if from_extension || from_native => match serde_json::from_slice::<AddRequest>(&req.body) {
            Ok(add) if is_http_url(&add.url) => {
                log::info!("add request from {}: {}", if from_extension { "the browser extension" } else { "a local client" }, add.url);
                manager.push_event(Event::AddRequest(add));
                respond(&mut stream, "200 OK", ext_origin, r#"{"ok":true}"#).await
            }
            _ => respond(&mut stream, "400 Bad Request", ext_origin, r#"{"ok":false,"error":"bad request"}"#).await,
        },
        ("POST", "/cli") if from_native => match serde_json::from_slice::<CliBody>(&req.body) {
            Ok(cli) => {
                for event in parse_cli(&cli.args) {
                    manager.push_event(event);
                }
                respond(&mut stream, "200 OK", None, r#"{"ok":true}"#).await
            }
            Err(_) => respond(&mut stream, "400 Bad Request", None, r#"{"ok":false}"#).await,
        },
        _ => respond(&mut stream, "403 Forbidden", None, r#"{"ok":false,"error":"forbidden"}"#).await,
    }
}

fn is_http_url(url: &str) -> bool {
    url::Url::parse(url).map(|u| matches!(u.scheme(), "http" | "https")).unwrap_or(false)
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

/// Hand our command line to an instance that is already running.
/// Returns false if nothing that looks like RDM answers on the port.
pub fn forward_to_running(port: u16, args: &[String]) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let body = serde_json::json!({ "args": args }).to_string();
    let req = format!(
        "POST /cli HTTP/1.1\r\nHost: 127.0.0.1\r\nX-RDM-Client: cli\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    resp.starts_with("HTTP/1.1 200") && resp.contains(r#""ok":true"#)
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
    }

    #[test]
    fn origins() {
        assert!(is_extension_origin("chrome-extension://abcdefghijklmnop"));
        assert!(is_extension_origin("moz-extension://1234-5678"));
        assert!(!is_extension_origin("https://evil.example"));
        assert!(!is_extension_origin("null"));
    }
}
