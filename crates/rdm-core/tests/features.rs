//! Features around the engine: checksums, lists, schedules, site logins and
//! limits, proxies, slow-connection eviction, changing an address.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Timelike;
use common::{Fixture, MIB};
use rdm_core::http::{build_client, NetConfig, ProxyConfig, RequestSpec};
use rdm_core::manager::{Event, SiteLimit, SiteLogin, MAIN_QUEUE};
use rdm_core::task::{self, Connections, Outcome, Start, TaskConfig};
use rdm_core::testserver::{fill, verify_file, ServerOptions, TestServer};
use rdm_core::{NewDownload, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn sha256_of(seed: u64, size: u64) -> String {
    use sha2::Digest;
    let mut data = vec![0u8; size as usize];
    fill(seed, 0, &mut data);
    hex::encode(sha2::Sha256::digest(&data))
}

#[test]
fn checksum_is_verified_after_download() {
    let fx = Fixture::new("checksum");
    let opts = ServerOptions::new(2 * MIB + 5);
    let (seed, size) = (opts.seed, opts.size);
    let server = fx.server(opts);

    let mut good = NewDownload::new(RequestSpec::new(server.url()));
    good.file_name = Some("good.bin".into());
    good.expected_hash = Some(format!("sha256:{}", sha256_of(seed, size)));
    let good = fx.manager.add(good);

    let mut bad = NewDownload::new(RequestSpec::new(server.url()));
    bad.file_name = Some("bad.bin".into());
    bad.expected_hash = Some("0".repeat(64));
    let bad = fx.manager.add(bad);

    fx.wait_status(good, Status::Complete, 20);
    assert_eq!(fx.manager.view(good).unwrap().d.verified, Some(true));

    fx.wait_status(bad, Status::Failed, 20);
    let v = fx.manager.view(bad).unwrap();
    assert_eq!(v.d.verified, Some(false));
    assert!(v.d.error.as_deref().unwrap_or("").starts_with("SHA-256 mismatch"), "{:?}", v.d.error);
    // The data itself is intact and kept, so the user can inspect it.
    verify_file(&v.d.path(), seed, size).unwrap();
}

#[test]
fn export_then_import_round_trips_the_list() {
    let fx = Fixture::new("export");
    for (i, name) in ["one.zip", "two.pdf", "three.mp4"].iter().enumerate() {
        let mut new = NewDownload::new(RequestSpec::new(format!("http://127.0.0.1:9/files/{name}")));
        new.description = format!("note {i}");
        new.start = false;
        fx.manager.add(new);
    }
    let json = fx.root.join("list.json");
    let txt = fx.root.join("list.txt");
    assert_eq!(fx.manager.export_list(&json).unwrap(), 3);
    assert_eq!(fx.manager.export_list(&txt).unwrap(), 3);
    assert_eq!(std::fs::read_to_string(&txt).unwrap().lines().count(), 3);

    // Importing into the same list adds nothing: every address is already there.
    assert_eq!(fx.manager.import_list(&json).unwrap(), 0);

    let other = Fixture::new("import");
    assert_eq!(other.manager.import_list(&json).unwrap(), 3);
    let views = other.manager.views();
    assert_eq!(views.len(), 3);
    assert!(views.iter().all(|v| v.d.status == Status::Queued), "imported downloads must not start by themselves");
    let two = views.iter().find(|v| v.d.file_name == "two.pdf").expect("names survive the round trip");
    assert_eq!(two.d.description, "note 1");
    // The plain-text form works too and is de-duplicated against the list.
    assert_eq!(other.manager.import_list(&txt).unwrap(), 0);

    let shortcut = fx.root.join("link.url");
    std::fs::write(&shortcut, "[InternetShortcut]\r\nURL=http://127.0.0.1:9/files/four.iso\r\n").unwrap();
    assert_eq!(other.manager.import_list(&shortcut).unwrap(), 1);
    assert!(other.manager.views().iter().any(|v| v.d.file_name == "four.iso" && v.d.category == "Compressed"));
}

#[test]
fn schedule_starts_the_queue_at_its_time() {
    let fx = Fixture::new("schedule");
    let server = fx.server(ServerOptions::new(MIB));

    let mut new = NewDownload::new(RequestSpec::new(server.url()));
    new.file_name = Some("nightly.bin".into());
    new.queue = Some(MAIN_QUEUE);
    new.start = false;
    let id = fx.manager.add(new);

    // Avoid arming the schedule in the last seconds of a minute, where "now"
    // could tick over before the scheduler looks.
    while chrono::Local::now().second() > 55 {
        std::thread::sleep(Duration::from_millis(200));
    }
    let now = chrono::Local::now();
    let mut q = fx.manager.queues().into_iter().find(|q| q.id == MAIN_QUEUE).unwrap();
    assert!(!q.running);

    // First a schedule for a different minute: nothing may happen.
    q.schedule.start_enabled = true;
    (q.schedule.start_hour, q.schedule.start_minute) = ((now.hour() + 2) % 24, now.minute());
    fx.manager.update_queue(q.clone());
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(fx.manager.view(id).unwrap().d.status, Status::Queued);

    // Now this very minute: the scheduler must start the queue by itself.
    (q.schedule.start_hour, q.schedule.start_minute) = (now.hour(), now.minute());
    fx.manager.update_queue(q);
    fx.wait_status(id, Status::Complete, 15);
    fx.wait_event(10, |e| matches!(e, Event::QueueFinished { queue: MAIN_QUEUE, .. }));
}

#[test]
fn site_login_is_sent_only_where_configured() {
    let fx = Fixture::new("login");
    let mut opts = ServerOptions::new(MIB);
    opts.require_auth = Some(("alice".into(), "s3cret".into()));
    let server = fx.server(opts);

    let mut new = NewDownload::new(RequestSpec::new(server.url()));
    new.file_name = Some("private-1.bin".into());
    let denied = fx.manager.add(new);
    fx.wait_status(denied, Status::Failed, 15);
    assert_eq!(fx.manager.view(denied).unwrap().d.error.as_deref(), Some("server returned HTTP 401"));

    let mut cfg = fx.manager.config();
    cfg.site_logins.push(SiteLogin { host: "127.0.0.1".into(), user: "alice".into(), password: "s3cret".into() });
    fx.manager.set_config(cfg).unwrap();

    let mut new = NewDownload::new(RequestSpec::new(server.url()));
    new.file_name = Some("private-2.bin".into());
    let allowed = fx.manager.add(new);
    fx.wait_status(allowed, Status::Complete, 15);
    // The failed one succeeds too once retried with the login in place.
    fx.manager.start(denied);
    fx.wait_status(denied, Status::Complete, 15);
}

#[test]
fn per_site_connection_limit_is_respected() {
    let fx = Fixture::new("sitelimit");
    let mut opts = ServerOptions::new(12 * MIB);
    opts.per_conn_rate = 4 * MIB;
    let (seed, size) = (opts.seed, opts.size);
    let server = fx.server(opts);

    let mut cfg = fx.manager.config();
    cfg.connections = Connections::Fixed(16);
    cfg.site_limits.push(SiteLimit { host: "127.0.0.1".into(), connections: 2 });
    fx.manager.set_config(cfg).unwrap();

    let mut new = NewDownload::new(RequestSpec::new(server.url()));
    new.file_name = Some("limited.bin".into());
    let id = fx.manager.add(new);
    fx.wait_status(id, Status::Complete, 30);
    verify_file(&fx.manager.view(id).unwrap().d.path(), seed, size).unwrap();
    let peak = server.stats.peak_concurrent.load(Ordering::Relaxed);
    // 3 is possible for an instant while the abandoned probe response winds down.
    assert!((2..=3).contains(&peak), "peak concurrency {peak} with a limit of 2");
}

#[test]
fn changing_the_address_keeps_progress() {
    let fx = Fixture::new("seturl");
    let mut opts = ServerOptions::new(10 * MIB + 3);
    opts.per_conn_rate = 512 * 1024;
    let (seed, size) = (opts.seed, opts.size);
    let old_server = fx.server(opts.clone());

    let mut new = NewDownload::new(RequestSpec::new(old_server.url()));
    new.file_name = Some("moved.bin".into());
    new.connections = Some(Connections::Fixed(8));
    let id = fx.manager.add(new);
    fx.wait_bytes(id, 3 * MIB, 20);
    fx.manager.pause(id);
    fx.wait_status(id, Status::Paused, 10);
    let kept = fx.manager.view(id).unwrap().d.downloaded;

    // The old link dies; the same file is now served from somewhere else.
    drop(old_server);
    opts.per_conn_rate = 0;
    let new_server = fx.server(opts);
    fx.manager.set_url(id, new_server.url());
    fx.manager.start(id);
    fx.wait_status(id, Status::Complete, 20);

    verify_file(&fx.manager.view(id).unwrap().d.path(), seed, size).unwrap();
    let fetched = new_server.stats.bytes_sent.load(Ordering::Relaxed);
    assert!(fetched <= size - kept + 2 * MIB, "re-fetched {fetched} bytes although {kept} were already on disk");
}

/// A minimal forwarding HTTP proxy that requires credentials and counts what passes through.
async fn start_proxy(user: &str, pass: &str) -> (std::net::SocketAddr, Arc<AtomicU64>, Arc<AtomicU64>) {
    use base64::Engine;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let want = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")));
    let (forwarded, refused) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (f, r) = (forwarded.clone(), refused.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else { break };
            let (want, f, r) = (want.clone(), f.clone(), r.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match client.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let authorized = text.lines().any(|l| {
                    l.split_once(':').is_some_and(|(k, v)| k.eq_ignore_ascii_case("proxy-authorization") && v.trim() == want)
                });
                if !authorized {
                    r.fetch_add(1, Ordering::Relaxed);
                    let _ = client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"p\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    return;
                }
                // "GET http://host:port/path HTTP/1.1" — connect to host:port and relay.
                let target = text.split_whitespace().nth(1).and_then(|u| url::Url::parse(u).ok());
                let Some(target) = target else { return };
                let host = format!("{}:{}", target.host_str().unwrap_or(""), target.port_or_known_default().unwrap_or(80));
                let Ok(mut upstream) = tokio::net::TcpStream::connect(&host).await else { return };
                f.fetch_add(1, Ordering::Relaxed);
                if upstream.write_all(&head).await.is_err() {
                    return;
                }
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (addr, forwarded, refused)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_through_an_authenticating_proxy() {
    let opts = ServerOptions::new(6 * MIB + 1);
    let (seed, size) = (opts.seed, opts.size);
    let server = TestServer::start(opts).await;
    let (proxy, forwarded, refused) = start_proxy("puser", "ppass").await;
    let dir = std::env::temp_dir().join("rdm-tests").join(format!("proxy-{}-{}", std::process::id(), fastrand::u32(..)));
    std::fs::create_dir_all(&dir).unwrap();

    let via = |user: &str, pass: &str| {
        build_client(&NetConfig {
            proxy: ProxyConfig::Manual { url: format!("http://{proxy}"), username: user.into(), password: pass.into() },
            ..Default::default()
        })
        .unwrap()
    };

    // Wrong password: the proxy refuses, and that is reported rather than retried forever.
    let mut cfg = TaskConfig::new(RequestSpec::new(server.url()), dir.join("denied.bin"));
    cfg.max_retries = 1;
    let outcome = task::spawn(via("puser", "wrong"), cfg, Start::New).wait().await;
    assert_eq!(outcome, Outcome::Failed("server returned HTTP 407".into()));
    assert!(refused.load(Ordering::Relaxed) >= 1);
    assert_eq!(server.stats.requests.load(Ordering::Relaxed), 0, "nothing may reach the server around the proxy");

    let path = dir.join("ok.bin");
    let mut cfg = TaskConfig::new(RequestSpec::new(server.url()), path.clone());
    cfg.connections = Connections::Fixed(4);
    assert_eq!(task::spawn(via("puser", "ppass"), cfg, Start::New).wait().await, Outcome::Complete);
    verify_file(&path, seed, size).unwrap();
    // Every connection went through the proxy.
    assert_eq!(forwarded.load(Ordering::Relaxed), server.stats.connections.load(Ordering::Relaxed));
    assert!(forwarded.load(Ordering::Relaxed) >= 4);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crawling_connection_is_replaced() {
    // One of four connections lands on a route that delivers 8 KB/s; the
    // others get 2 MB/s. Its 12 MiB share would take 25 minutes.
    let mut opts = ServerOptions::new(48 * MIB);
    opts.per_conn_rate = 2 * MIB;
    opts.slow_connections = 1;
    opts.slow_rate = 8 * 1024;
    let (seed, size) = (opts.seed, opts.size);
    let server = TestServer::start(opts).await;
    let dir = std::env::temp_dir().join("rdm-tests").join(format!("evict-{}-{}", std::process::id(), fastrand::u32(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("out.bin");

    let client = build_client(&NetConfig { proxy: ProxyConfig::None, ..Default::default() }).unwrap();
    let mut cfg = TaskConfig::new(RequestSpec::new(server.url()), path.clone());
    cfg.connections = Connections::Fixed(4);
    cfg.evict_settle = Duration::from_secs(1);
    cfg.evict_after_secs = 2;

    let t = Instant::now();
    assert_eq!(task::spawn(client, cfg, Start::New).wait().await, Outcome::Complete);
    let secs = t.elapsed().as_secs_f64();
    verify_file(&path, seed, size).unwrap();
    println!("with one crawling connection: {secs:.1}s, {} connections used", server.stats.connections.load(Ordering::Relaxed));
    // Healthy: 48 MiB at 8 MiB/s = 6 s. Leaving the crawler alone would take minutes.
    assert!(secs < 14.0, "took {secs:.1}s — the slow connection was not replaced");
    assert!(server.stats.connections.load(Ordering::Relaxed) > 4, "a replacement connection should have been opened");
    let _ = std::fs::remove_dir_all(dir);
}

/// Serve one fixed HTML page on every path.
async fn serve_page(html: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", html.len());
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(html.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/gallery/index.html")
}

#[test]
fn link_grabber_lists_the_files_on_a_page() {
    let fx = Fixture::new("grab");
    let page = r#"<html><body>
        <a href="files/setup.exe">Installer</a>
        <a href="/docs/manual.pdf">Manual</a>
        <a href="about.html">About</a>
        <img src="logo.png">
        <a href="https://cdn.example.org/media/trailer.mp4?sig=abc">Trailer</a>
    </body></html>"#;
    let url = fx.manager.runtime().block_on(serve_page(page));

    let wait = |ticket: rdm_core::manager::GrabTicket| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = ticket.result() {
                return result;
            }
            assert!(Instant::now() < deadline, "the grabber never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    let wanted = vec!["exe".to_string(), "pdf".to_string(), "mp4".to_string()];
    let links = wait(fx.manager.grab(RequestSpec::new(url.clone()), wanted)).unwrap();
    let names: Vec<&str> = links.iter().map(|l| l.file_name.as_str()).collect();
    assert_eq!(names, vec!["setup.exe", "manual.pdf", "trailer.mp4"]);
    let base = url.trim_end_matches("/gallery/index.html");
    assert_eq!(links[0].url, format!("{base}/gallery/files/setup.exe"));
    assert_eq!(links[1].url, format!("{base}/docs/manual.pdf"));
    assert_eq!(links[2].url, "https://cdn.example.org/media/trailer.mp4?sig=abc");

    // No filter: every linked file, pages without an extension excluded.
    let all = wait(fx.manager.grab(RequestSpec::new(url), Vec::new())).unwrap();
    assert_eq!(all.len(), 5);

    // A dead address reports an error instead of hanging.
    let err = wait(fx.manager.grab(RequestSpec::new("http://127.0.0.1:9/nothing"), Vec::new())).unwrap_err();
    assert!(!err.is_empty());
}
