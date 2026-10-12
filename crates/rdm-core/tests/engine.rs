//! End-to-end engine tests against the fault-injecting server (SPEC.md §12, M1).

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rdm_core::filename::{part_path, state_path};
use rdm_core::http::{build_client, NetConfig, ProxyConfig, RequestSpec};
use rdm_core::state::ResumeState;
use rdm_core::task::{self, Connections, Outcome, Start, TaskConfig, TaskHandle};
use rdm_core::testserver::{verify_file, ServerOptions, TestServer};
use rdm_core::RateLimiter;

const MIB: u64 = 1024 * 1024;

fn client() -> reqwest::Client {
    // Loopback must not go through whatever proxy the machine has configured.
    build_client(&NetConfig { proxy: ProxyConfig::None, ..Default::default() }).unwrap()
}

fn temp_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("rdm-tests").join(format!("{name}-{}-{}", std::process::id(), fastrand::u32(..)));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("out.bin")
}

fn config(server: &TestServer, path: &PathBuf, conns: Connections) -> TaskConfig {
    let mut cfg = TaskConfig::new(RequestSpec::new(server.url()), path.clone());
    cfg.connections = conns;
    cfg.stall_timeout = Duration::from_secs(5);
    cfg
}

fn start(cfg: TaskConfig, how: Start) -> TaskHandle {
    task::spawn(client(), cfg, how)
}

async fn wait_for_bytes(handle: &TaskHandle, bytes: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while handle.progress.downloaded() < bytes {
        assert!(Instant::now() < deadline, "download never reached {bytes} bytes");
        assert!(!handle.is_finished(), "download ended before reaching {bytes} bytes");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

async fn download_and_verify(opts: ServerOptions, conns: Connections, name: &str) -> (TestServer, Arc<rdm_core::Progress>) {
    let (seed, size) = (opts.seed, opts.size);
    let server = TestServer::start(opts).await;
    let path = temp_file(name);
    let handle = start(config(&server, &path, conns), Start::New);
    let progress = handle.progress.clone();
    assert_eq!(handle.wait().await, Outcome::Complete);
    verify_file(&path, seed, size).unwrap();
    assert!(!part_path(&path).exists(), "part file left behind");
    assert!(!state_path(&path).exists(), "state file left behind");
    assert_eq!(progress.downloaded(), size);
    cleanup(&path);
    (server, progress)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_connection_is_byte_exact() {
    let (server, _) = download_and_verify(ServerOptions::new(3 * MIB + 17), Connections::Fixed(1), "one").await;
    assert_eq!(server.stats.peak_concurrent.load(Ordering::Relaxed), 1);
    // The probe response is reused: a single-connection download is one request.
    assert_eq!(server.stats.requests.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_connections_are_byte_exact() {
    let mut opts = ServerOptions::new(24 * MIB + 3);
    opts.per_conn_rate = 6 * MIB;
    let (server, progress) = download_and_verify(opts, Connections::Fixed(8), "eight").await;
    // 9 is possible for an instant: the server notices the abandoned probe
    // response a moment after its connection has already moved on.
    let peak = server.stats.peak_concurrent.load(Ordering::Relaxed);
    assert!((8..=9).contains(&peak), "peak concurrency {peak}");
    assert!(progress.resumable());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thirty_two_connections_are_byte_exact() {
    let mut opts = ServerOptions::new(48 * MIB + 5);
    opts.per_conn_rate = 2 * MIB;
    let (server, _) = download_and_verify(opts, Connections::Fixed(32), "thirtytwo").await;
    let peak = server.stats.peak_concurrent.load(Ordering::Relaxed);
    assert!((30..=33).contains(&peak), "peak concurrency {peak}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiny_and_empty_files() {
    download_and_verify(ServerOptions::new(1), Connections::Auto, "tiny1").await;
    download_and_verify(ServerOptions::new(1000), Connections::Fixed(32), "tiny1000").await;
    download_and_verify(ServerOptions::new(0), Connections::Auto, "empty").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_mode_adds_connections_while_they_help() {
    let mut opts = ServerOptions::new(40 * MIB);
    opts.per_conn_rate = 400 * 1024;
    let (server, _) = download_and_verify(opts, Connections::Auto, "auto").await;
    let peak = server.stats.peak_concurrent.load(Ordering::Relaxed);
    assert!(peak >= 18, "auto mode should have grown well past 8 connections, peak was {peak}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segmented_beats_single_on_throttled_server() {
    let mut opts = ServerOptions::new(12 * MIB);
    opts.per_conn_rate = 2 * MIB;

    let t = Instant::now();
    download_and_verify(opts.clone(), Connections::Fixed(1), "throttle1").await;
    let single = t.elapsed().as_secs_f64();

    let t = Instant::now();
    download_and_verify(opts, Connections::Fixed(8), "throttle8").await;
    let eight = t.elapsed().as_secs_f64();

    let speedup = single / eight;
    println!("single {single:.2}s, eight {eight:.2}s, speedup {speedup:.1}x");
    assert!(speedup >= 5.0, "expected >= 5x, got {speedup:.1}x (single {single:.2}s, eight {eight:.2}s)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_then_resume_keeps_progress() {
    let mut opts = ServerOptions::new(16 * MIB + 11);
    opts.per_conn_rate = 512 * 1024;
    let (seed, size) = (opts.seed, opts.size);
    let server = TestServer::start(opts).await;
    let path = temp_file("pause");

    let handle = start(config(&server, &path, Connections::Fixed(8)), Start::New);
    wait_for_bytes(&handle, 5 * MIB).await;
    handle.pause();
    let progress = handle.progress.clone();
    assert_eq!(handle.wait().await, Outcome::Paused);

    assert!(part_path(&path).exists());
    assert!(!path.exists());
    let state = ResumeState::load(&state_path(&path)).expect("resume state saved on pause");
    let kept = state.downloaded();
    assert!(kept >= 5 * MIB, "only {kept} bytes recorded");
    assert_eq!(progress.downloaded(), kept);

    let sent_before = server.stats.bytes_sent.load(Ordering::Relaxed);
    let handle = start(config(&server, &path, Connections::Fixed(8)), Start::Resume(state));
    assert_eq!(handle.wait().await, Outcome::Complete);
    verify_file(&path, seed, size).unwrap();

    // Resuming must fetch only what was missing (plus in-flight slack), not the file again.
    let refetched = server.stats.bytes_sent.load(Ordering::Relaxed) - sent_before;
    assert!(refetched <= size - kept + 3 * MIB, "resume fetched {refetched} bytes for {} missing", size - kept);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_then_resume_from_last_checkpoint() {
    let mut opts = ServerOptions::new(24 * MIB + 1);
    opts.per_conn_rate = 512 * 1024;
    let (seed, size) = (opts.seed, opts.size);
    let server = TestServer::start(opts).await;
    let path = temp_file("crash");

    let handle = start(config(&server, &path, Connections::Fixed(8)), Start::New);
    // Checkpoints are written every 3 s; run past the first one, then die without saving.
    tokio::time::sleep(Duration::from_millis(4200)).await;
    handle.kill();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let state = ResumeState::load(&state_path(&path)).expect("periodic checkpoint on disk");
    let kept = state.downloaded();
    assert!(kept >= 8 * MIB, "checkpoint recorded only {kept} bytes");

    let handle = start(config(&server, &path, Connections::Fixed(8)), Start::Resume(state));
    assert_eq!(handle.wait().await, Outcome::Complete);
    verify_file(&path, seed, size).unwrap();
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn survives_connections_dropped_mid_stream() {
    let mut opts = ServerOptions::new(6 * MIB + 7);
    opts.drop_after = Some(300 * 1024);
    opts.drop_count = 14;
    let (server, _) = download_and_verify(opts, Connections::Fixed(4), "drops").await;
    assert!(server.stats.requests.load(Ordering::Relaxed) > 14);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_without_range_support() {
    let mut opts = ServerOptions::new(5 * MIB + 1);
    opts.ranges = false;
    let (server, progress) = download_and_verify(opts, Connections::Fixed(8), "norange").await;
    assert!(!progress.resumable());
    assert_eq!(server.stats.peak_concurrent.load(Ordering::Relaxed), 1);
    assert_eq!(server.stats.requests.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_without_content_length() {
    let mut opts = ServerOptions::new(4 * MIB + 9);
    opts.ranges = false;
    opts.send_length = false;
    let (_, progress) = download_and_verify(opts, Connections::Auto, "nolength").await;
    assert_eq!(progress.size(), Some(4 * MIB + 9));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_range_download_restarts_after_a_drop() {
    let mut opts = ServerOptions::new(3 * MIB);
    opts.ranges = false;
    opts.drop_after = Some(MIB);
    opts.drop_count = 2;
    download_and_verify(opts, Connections::Auto, "norange-drop").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_file_is_never_spliced() {
    let mut opts = ServerOptions::new(12 * MIB);
    opts.per_conn_rate = 512 * 1024;
    let server = TestServer::start(opts).await;
    let path = temp_file("changed");

    let handle = start(config(&server, &path, Connections::Fixed(8)), Start::New);
    wait_for_bytes(&handle, 3 * MIB).await;
    handle.pause();
    assert_eq!(handle.wait().await, Outcome::Paused);
    let state = ResumeState::load(&state_path(&path)).unwrap();

    // Same size, different content and a new ETag.
    server.update(|o| {
        o.seed = 0xC0FFEE;
        o.etag = Some("\"v2\"".into());
        o.per_conn_rate = 0;
    });

    let handle = start(config(&server, &path, Connections::Fixed(8)), Start::Resume(state));
    assert_eq!(handle.wait().await, Outcome::Complete);
    verify_file(&path, 0xC0FFEE, 12 * MIB).unwrap();
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapts_to_server_connection_cap() {
    let mut opts = ServerOptions::new(12 * MIB + 5);
    opts.per_conn_rate = 2 * MIB;
    opts.max_concurrent = 3;
    let (server, _) = download_and_verify(opts, Connections::Fixed(8), "cap").await;
    assert_eq!(server.stats.peak_concurrent.load(Ordering::Relaxed), 3);
    let rejected = server.stats.rejected.load(Ordering::Relaxed);
    // It must learn the cap rather than hammer the server for the whole download.
    assert!((1..=12).contains(&rejected), "{rejected} rejected requests");
}

/// Modelled on a real CDN: only a few requests per time window are accepted,
/// the rest get `429 Retry-After`. The engine must wait as told and space its
/// requests out — not retry in a tight loop, and not collapse to one connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backs_off_when_rate_limited() {
    let mut opts = ServerOptions::new(24 * MIB + 9);
    opts.per_conn_rate = 2 * MIB;
    opts.request_limit = Some((4, Duration::from_secs(2)));
    let t = Instant::now();
    let (server, _) = download_and_verify(opts, Connections::Fixed(8), "ratelimit").await;
    let secs = t.elapsed().as_secs_f64();

    let rejected = server.stats.rejected.load(Ordering::Relaxed);
    let requests = server.stats.requests.load(Ordering::Relaxed);
    let peak = server.stats.peak_concurrent.load(Ordering::Relaxed);
    println!("rate-limited: {secs:.1}s, {requests} requests, {rejected} rejected, peak {peak}");
    // 4 of the first 8 are refused; after that rejections must be rare.
    assert!((4..=10).contains(&rejected), "{rejected} rejected of {requests} requests");
    assert!(peak >= 4, "should keep using the connections the server allows, peak was {peak}");
    // One connection alone would need 12 s.
    assert!(secs < 8.0, "took {secs:.1}s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_file_fails_fast() {
    let mut opts = ServerOptions::new(MIB);
    opts.force_status = Some(404);
    let server = TestServer::start(opts).await;
    let path = temp_file("404");
    let t = Instant::now();
    let outcome = start(config(&server, &path, Connections::Auto), Start::New).wait().await;
    assert_eq!(outcome, Outcome::Failed("server returned HTTP 404".into()));
    assert!(t.elapsed() < Duration::from_secs(2), "a 404 must not be retried");
    assert!(!part_path(&path).exists());
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreachable_server_gives_up_after_retries() {
    // Bind then drop, so the port is very likely closed.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let path = temp_file("refused");
    let mut cfg = TaskConfig::new(RequestSpec::new(format!("http://127.0.0.1:{port}/x.bin")), path.clone());
    cfg.max_retries = 2;
    let outcome = start(cfg, Start::New).wait().await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speed_limit_is_respected() {
    let opts = ServerOptions::new(4 * MIB);
    let (seed, size) = (opts.seed, opts.size);
    let server = TestServer::start(opts).await;
    let path = temp_file("limit");
    let mut cfg = config(&server, &path, Connections::Fixed(8));
    cfg.limiter = Arc::new(RateLimiter::new(MIB));

    let t = Instant::now();
    assert_eq!(start(cfg, Start::New).wait().await, Outcome::Complete);
    let secs = t.elapsed().as_secs_f64();
    verify_file(&path, seed, size).unwrap();
    // 4 MiB at 1 MiB/s: 4 s, within the ±5 % the spec asks for (plus start-up slack).
    assert!((3.7..4.4).contains(&secs), "took {secs:.2}s");
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_name_comes_from_content_disposition() {
    let mut opts = ServerOptions::new(1024);
    opts.file_name = Some("Annual Report 2026.pdf".into());
    let server = TestServer::start(opts).await;
    let probe = rdm_core::http::probe(&client(), &RequestSpec::new(server.url())).await.unwrap();
    assert_eq!(probe.info.file_name, "Annual Report 2026.pdf");
    assert_eq!(probe.info.size, Some(1024));
    assert!(probe.info.resumable);
}
