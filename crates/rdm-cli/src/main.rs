//! `rdmc` — headless front end for the RDM engine: downloads, benchmarks, test server.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rdm_core::filename::{state_path, unique_path};
use rdm_core::http::{self, build_client, NetConfig, ProxyConfig, RequestSpec};
use rdm_core::state::ResumeState;
use rdm_core::task::{self, Connections, Outcome, Start, TaskConfig};
use rdm_core::testserver::{verify_file, ServerOptions, TestServer};
use rdm_core::util::{eta_secs, format_duration, format_size, format_speed, parse_size};
use rdm_core::RateLimiter;

const USAGE: &str = "\
rdmc — Rapid Download Manager, command line

USAGE:
  rdmc get <url> [options]      Download a file (re-run the same command to resume)
      -o, --output <path>       File or directory to save to (default: current directory)
      -c, --connections <n>     1-32, or 'auto' (default)
      -l, --limit <rate>        Speed limit, e.g. 500k, 2M (per second)
          --referer <url>
          --cookie <string>
      -H, --header <'K: V'>     Extra request header (repeatable)
          --user <name:pass>    HTTP Basic credentials
          --no-proxy            Ignore the system proxy

  rdmc bench [options]          Measure the engine against a local server
      -s, --size <size>         File size (default 1G)
      -r, --rate <rate>         Per-connection server throttle (default: none)
      -c, --connections <n>     Connections to compare against 1 (default auto)
          --cap <n>             Server-side concurrent connection cap

  rdmc serve [options]          Run the test server until Ctrl+C
      -s, --size <size>   -r, --rate <rate>   -p, --port <port>   --cap <n>
";

/// `-v` / `--verbose` anywhere on the command line prints engine decisions to stderr.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("rdm")
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("\n[{}] {}", record.level(), record.args());
        }
    }
    fn flush(&self) {}
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-v" || a == "--verbose") {
        args.retain(|a| a != "-v" && a != "--verbose");
        let _ = log::set_logger(&StderrLog);
        log::set_max_level(log::LevelFilter::Debug);
    }
    let code = match args.first().map(String::as_str) {
        Some("get") => run(cmd_get(&args[1..])),
        Some("bench") => run(cmd_bench(&args[1..])),
        Some("serve") => run(cmd_serve(&args[1..])),
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            0
        }
        // `rdmc <url>` is shorthand for `rdmc get <url>`.
        Some(a) if a.starts_with("http://") || a.starts_with("https://") => run(cmd_get(&args)),
        Some(other) => {
            eprintln!("unknown command '{other}'\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn run(fut: impl std::future::Future<Output = Result<(), String>>) -> i32 {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    match rt.block_on(fut) {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("error: {msg}");
            1
        }
    }
}

/// Minimal flag parser: `--name value`, `-n value`, repeated flags, positionals.
struct Args<'a> {
    rest: std::slice::Iter<'a, String>,
}

enum Arg<'a> {
    Flag(&'a str),
    Positional(&'a str),
}

impl<'a> Args<'a> {
    fn new(args: &'a [String]) -> Self {
        Self { rest: args.iter() }
    }
    fn next(&mut self) -> Option<Arg<'a>> {
        let a = self.rest.next()?;
        Some(if a.starts_with('-') && a.len() > 1 { Arg::Flag(a) } else { Arg::Positional(a) })
    }
    fn value(&mut self, flag: &str) -> Result<&'a str, String> {
        self.rest.next().map(String::as_str).ok_or_else(|| format!("{flag} needs a value"))
    }
}

fn parse_connections(v: &str) -> Result<Connections, String> {
    if v.eq_ignore_ascii_case("auto") {
        return Ok(Connections::Auto);
    }
    match v.parse::<u32>() {
        Ok(n) if (1..=task::MAX_CONNECTIONS).contains(&n) => Ok(Connections::Fixed(n)),
        _ => Err(format!("connections must be 1-{} or 'auto', got '{v}'", task::MAX_CONNECTIONS)),
    }
}

fn parse_rate(v: &str) -> Result<u64, String> {
    parse_size(v).ok_or_else(|| format!("cannot parse '{v}' (try 500k, 2M, 1G)"))
}

async fn cmd_get(args: &[String]) -> Result<(), String> {
    let mut spec = RequestSpec::default();
    let mut output: Option<PathBuf> = None;
    let mut conns = Connections::Auto;
    let mut limit = 0u64;
    let mut net = NetConfig::default();

    let mut it = Args::new(args);
    while let Some(arg) = it.next() {
        match arg {
            Arg::Positional(url) if spec.url.is_empty() => spec.url = url.to_string(),
            Arg::Positional(extra) => return Err(format!("unexpected argument '{extra}'")),
            Arg::Flag(f @ ("-o" | "--output")) => output = Some(PathBuf::from(it.value(f)?)),
            Arg::Flag(f @ ("-c" | "--connections")) => conns = parse_connections(it.value(f)?)?,
            Arg::Flag(f @ ("-l" | "--limit")) => limit = parse_rate(it.value(f)?)?,
            Arg::Flag(f @ "--referer") => spec.referer = Some(it.value(f)?.to_string()),
            Arg::Flag(f @ "--cookie") => spec.cookies = Some(it.value(f)?.to_string()),
            Arg::Flag(f @ ("-H" | "--header")) => {
                let (k, v) = it.value(f)?.split_once(':').ok_or("header must look like 'Name: value'")?;
                spec.headers.push((k.trim().to_string(), v.trim().to_string()));
            }
            Arg::Flag(f @ "--user") => {
                let (u, p) = it.value(f)?.split_once(':').ok_or("--user must look like name:password")?;
                spec.auth = Some((u.to_string(), p.to_string()));
            }
            Arg::Flag("--no-proxy") => net.proxy = ProxyConfig::None,
            Arg::Flag(other) => return Err(format!("unknown option '{other}'")),
        }
    }
    if spec.url.is_empty() {
        return Err("no URL given".into());
    }
    let client = build_client(&net).map_err(|e| e.to_string())?;

    // An explicit file path lets us resume without asking the server anything.
    let explicit_file = output.as_ref().filter(|p| !p.is_dir()).cloned();
    let (final_path, start) = match explicit_file {
        Some(path) => match ResumeState::load(&state_path(&path)).filter(|s| s.url == spec.url) {
            Some(state) => (path, Start::Resume(state)),
            None => (path, Start::New),
        },
        None => {
            let dir = match output {
                Some(d) => d,
                None => std::env::current_dir().map_err(|e| e.to_string())?,
            };
            let probe = http::probe(&client, &spec).await.map_err(|e| e.to_string())?;
            let named = dir.join(&probe.info.file_name);
            match ResumeState::load(&state_path(&named)).filter(|s| s.url == spec.url) {
                Some(state) => (named, Start::Resume(state)),
                None => (unique_path(&dir, &probe.info.file_name, |_| false), Start::Fresh(probe)),
            }
        }
    };
    let resumed_bytes = match &start {
        Start::Resume(state) => {
            println!("Resuming {} ({} already downloaded)", final_path.display(), format_size(state.downloaded()));
            Some(state.downloaded())
        }
        _ => {
            println!("Saving to {}", final_path.display());
            None
        }
    };

    let mut cfg = TaskConfig::new(spec, final_path.clone());
    cfg.connections = conns;
    cfg.limiter = Arc::new(RateLimiter::new(limit));
    let handle = task::spawn(client, cfg, start);
    let progress = handle.progress.clone();

    let started = Instant::now();
    // Bytes already on disk from an earlier run, so the average covers this run only.
    let base = match &resumed_bytes {
        Some(bytes) => *bytes,
        None => 0,
    };
    let pauser = handle.pauser();
    let mut wait = Box::pin(handle.wait());
    let mut interrupts = 0;
    let outcome = loop {
        tokio::select! {
            outcome = &mut wait => break outcome,
            _ = tokio::signal::ctrl_c() => {
                interrupts += 1;
                if interrupts > 1 {
                    std::process::exit(130);
                }
                eprint!("\nPausing (progress is kept; run the same command to resume)…");
                pauser.pause();
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => print_progress(&progress),
        }
    };
    print_progress(&progress);
    println!();

    match outcome {
        Outcome::Complete => {
            let secs = started.elapsed().as_secs_f64().max(0.001);
            let fetched = progress.downloaded().saturating_sub(base);
            println!(
                "Done: {} in {:.1} s, average {}",
                format_size(progress.downloaded()),
                secs,
                format_speed((fetched as f64 / secs) as u64)
            );
            Ok(())
        }
        Outcome::Paused => {
            println!("Paused at {}.", format_size(progress.downloaded()));
            Ok(())
        }
        Outcome::Failed(msg) => Err(msg),
    }
}

fn print_progress(p: &rdm_core::Progress) {
    let (done, size, speed) = (p.downloaded(), p.size(), p.speed());
    let line = match size {
        Some(size) if size > 0 => format!(
            "{:5.1}%  {} / {}  {}  {} conn  {}",
            done as f64 * 100.0 / size as f64,
            format_size(done),
            format_size(size),
            format_speed(speed),
            p.connections(),
            eta_secs(Some(size), done, speed).map(|s| format!("ETA {}", format_duration(s))).unwrap_or_default(),
        ),
        _ => format!("{}  {}  {} conn", format_size(done), format_speed(speed), p.connections()),
    };
    let note = p.last_error().map(|e| format!("  [retrying: {e}]")).unwrap_or_default();
    print!("\r{:<78}", format!("{line}{note}"));
    let _ = std::io::stdout().flush();
}

struct ServeArgs {
    size: u64,
    rate: u64,
    port: u16,
    cap: usize,
    conns: Connections,
}

fn parse_serve_args(args: &[String]) -> Result<ServeArgs, String> {
    let mut out = ServeArgs { size: 1 << 30, rate: 0, port: 0, cap: 0, conns: Connections::Auto };
    let mut it = Args::new(args);
    while let Some(arg) = it.next() {
        match arg {
            Arg::Flag(f @ ("-s" | "--size")) => out.size = parse_rate(it.value(f)?)?,
            Arg::Flag(f @ ("-r" | "--rate")) => out.rate = parse_rate(it.value(f)?)?,
            Arg::Flag(f @ ("-p" | "--port")) => out.port = it.value(f)?.parse().map_err(|_| "bad port")?,
            Arg::Flag(f @ "--cap") => out.cap = it.value(f)?.parse().map_err(|_| "bad cap")?,
            Arg::Flag(f @ ("-c" | "--connections")) => out.conns = parse_connections(it.value(f)?)?,
            Arg::Flag(other) => return Err(format!("unknown option '{other}'")),
            Arg::Positional(extra) => return Err(format!("unexpected argument '{extra}'")),
        }
    }
    Ok(out)
}

fn server_options(a: &ServeArgs) -> ServerOptions {
    let mut opts = ServerOptions::new(a.size);
    opts.per_conn_rate = a.rate;
    opts.max_concurrent = a.cap;
    opts
}

async fn cmd_serve(args: &[String]) -> Result<(), String> {
    let a = parse_serve_args(args)?;
    let server = TestServer::start_on(server_options(&a), a.port).await;
    println!("Serving {} at {}", format_size(a.size), server.url());
    if a.rate > 0 {
        println!("Throttled to {} per connection", format_speed(a.rate));
    }
    println!("Ctrl+C to stop.");
    tokio::signal::ctrl_c().await.map_err(|e| e.to_string())?;
    let s = &server.stats;
    println!(
        "\n{} requests on {} connections, {} sent, peak concurrency {}",
        s.requests.load(Ordering::Relaxed),
        s.connections.load(Ordering::Relaxed),
        format_size(s.bytes_sent.load(Ordering::Relaxed)),
        s.peak_concurrent.load(Ordering::Relaxed)
    );
    Ok(())
}

async fn cmd_bench(args: &[String]) -> Result<(), String> {
    let a = parse_serve_args(args)?;
    let dir = std::env::temp_dir().join(format!("rdm-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let client = build_client(&NetConfig { proxy: ProxyConfig::None, ..Default::default() }).map_err(|e| e.to_string())?;

    println!(
        "File {}; server throttle per connection: {}; server cap: {}",
        format_size(a.size),
        if a.rate > 0 { format_speed(a.rate) } else { "none".into() },
        if a.cap > 0 { a.cap.to_string() } else { "none".into() },
    );

    let mut results = Vec::new();
    for conns in [Connections::Fixed(1), a.conns] {
        let opts = server_options(&a);
        let seed = opts.seed;
        let server = TestServer::start(opts).await;
        let path = dir.join(format!("bench-{}.bin", results.len()));
        let mut cfg = TaskConfig::new(RequestSpec::new(server.url()), path.clone());
        cfg.connections = conns;

        let t = Instant::now();
        let handle = task::spawn(client.clone(), cfg, Start::New);
        let outcome = handle.wait().await;
        let secs = t.elapsed().as_secs_f64();
        if outcome != Outcome::Complete {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("benchmark download did not complete: {outcome:?}"));
        }
        let verified = tokio::task::spawn_blocking({
            let path = path.clone();
            move || verify_file(&path, seed, a.size)
        })
        .await
        .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&path);
        if let Err(e) = verified {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("downloaded file is corrupt: {e}"));
        }

        let label = match conns {
            Connections::Fixed(n) => format!("{n} connection{}", if n == 1 { "" } else { "s" }),
            Connections::Auto => "auto".to_string(),
        };
        let rate = a.size as f64 / secs;
        println!(
            "  {label:<16} {secs:>7.2} s   {:>12}   peak {} connections, {} requests, verified OK",
            format_speed(rate as u64),
            server.stats.peak_concurrent.load(Ordering::Relaxed),
            server.stats.requests.load(Ordering::Relaxed),
        );
        results.push(secs);
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("  speed-up: {:.1}x", results[0] / results[1]);
    Ok(())
}
