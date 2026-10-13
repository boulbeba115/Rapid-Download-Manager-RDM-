//! `rdm-host.exe` — the browser's way in to Rapid Download Manager.
//!
//! The browser extension cannot (and must not) reach the application's
//! loopback port. Instead the browser starts this program through *native
//! messaging* and pipes JSON messages to it: each message is a 32-bit length
//! in native byte order followed by that many bytes of UTF-8 JSON, in both
//! directions. The browser only does so for the extension IDs listed in the
//! host manifest, which is what ties the extension to this installation.
//!
//! This program is deliberately small. It understands two requests, checks
//! every field, and forwards to the running application using the per-run
//! secret that only this Windows user can read:
//!
//! ```text
//! {"type":"status"}            → {"ok":true,"running":…,"version":…,"hostVersion":…,"captureExtensions":[…]}
//! {"type":"add","url":…,…}     → {"ok":true} | {"ok":false,"error":…}
//! ```
//!
//! `rdm-host --register` / `--unregister` add and remove the registration
//! (used by the installer; exit code 0 on success).

// Started by the browser for every message: it must never flash a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use rdm_core::{api, browser, Config};
use serde_json::{json, Value};

/// Far more than a link with its cookies needs; anything larger is not ours.
const MAX_MESSAGE: usize = 1024 * 1024;
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some(origin) if origin.starts_with("chrome-extension://") => {
            if !browser::is_allowed_origin(origin) {
                eprintln!("rdm-host: {origin} is not allowed to use this host");
                return ExitCode::from(3);
            }
            serve(&mut std::io::stdin().lock(), &mut std::io::stdout().lock());
            ExitCode::SUCCESS
        }
        Some("--register") => report(exe_dir().and_then(|dir| browser::register(&dir)).map(|m| format!("registered {}", m.display()))),
        Some("--unregister") => report(exe_dir().and_then(|dir| browser::unregister(&dir)).map(|()| "unregistered".to_string())),
        _ => {
            eprintln!("rdm-host {VERSION}: started by the browser for the RDM extension.\nusage: rdm-host --register | --unregister");
            ExitCode::from(2)
        }
    }
}

fn report(result: std::io::Result<String>) -> ExitCode {
    match result {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rdm-host: {e}");
            ExitCode::FAILURE
        }
    }
}

fn exe_dir() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.parent().map(PathBuf::from).ok_or_else(|| std::io::Error::other("the executable has no parent folder"))
}

// ---------------------------------------------------------------- Native messaging

fn read_message(input: &mut impl Read) -> std::io::Result<Option<Value>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        // The browser closed the pipe: we are done.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message size out of range"));
    }
    let mut buf = vec![0u8; len];
    input.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map(Some).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn write_message(output: &mut impl Write, value: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    output.write_all(&(bytes.len() as u32).to_ne_bytes())?;
    output.write_all(&bytes)?;
    output.flush()
}

/// Answer messages until the browser hangs up. Anything malformed ends the session.
fn serve(input: &mut impl Read, output: &mut impl Write) {
    let app = App::locate();
    while let Ok(Some(message)) = read_message(input) {
        if write_message(output, &handle(&app, &message)).is_err() {
            break;
        }
    }
}

fn handle(app: &App, message: &Value) -> Value {
    match message.get("type").and_then(Value::as_str) {
        Some("status") => app.status(),
        Some("add") => match app.add(message) {
            Ok(()) => json!({ "ok": true }),
            Err(error) => json!({ "ok": false, "error": error }),
        },
        _ => json!({ "ok": false, "error": "unknown request" }),
    }
}

// ---------------------------------------------------------------- The application

struct App {
    data_dir: PathBuf,
}

impl App {
    fn locate() -> Self {
        Self { data_dir: rdm_core::state::default_data_dir() }
    }

    fn config(&self) -> Config {
        rdm_core::state::load_json(&self.data_dir.join("config.json")).unwrap_or_default()
    }

    /// One request to the running application; `None` if it is not there (or is
    /// someone else's: a wrong secret counts as not there).
    fn call(&self, method: &str, path: &str, body: Option<&str>) -> Option<Value> {
        let token = api::read_token(&self.data_dir)?;
        let reply = api::call(self.config().api_port, &token, method, path, body).ok()?;
        if reply.status == 403 {
            return None;
        }
        serde_json::from_str(&reply.body).ok()
    }

    fn running_version(&self) -> Option<String> {
        let ping = self.call("GET", "/ping", None)?;
        (ping.get("app")?.as_str()? == "rdm").then(|| ping.get("version").and_then(Value::as_str).unwrap_or("").to_string())
    }

    fn status(&self) -> Value {
        let version = self.running_version();
        // The list of file types comes from the application when it runs, from its settings file otherwise.
        let live = version.as_ref().and_then(|_| self.call("GET", "/config", None)).and_then(|c| c.get("captureExtensions").cloned());
        let extensions = live.unwrap_or_else(|| json!(self.config().capture_extensions));
        json!({
            "ok": true,
            "running": version.is_some(),
            "version": version,
            "hostVersion": VERSION,
            "captureExtensions": extensions,
        })
    }

    /// Start the application next to this program, hidden in the notification
    /// area, and wait until it answers.
    fn start(&self) -> Result<(), String> {
        let exe = exe_dir().map_err(|e| e.to_string())?.join("rdm.exe");
        if !exe.is_file() {
            return Err("Rapid Download Manager is not installed next to its browser host".into());
        }
        spawn_detached(&exe).map_err(|e| format!("cannot start Rapid Download Manager: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.running_version().is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        Err("Rapid Download Manager did not start in time".into())
    }

    fn add(&self, message: &Value) -> Result<(), String> {
        let field = |name: &str| message.get(name).and_then(Value::as_str).filter(|v| !v.is_empty());
        let url = field("url").filter(|u| u.starts_with("http://") || u.starts_with("https://")).ok_or("not a web address")?;
        // Only these fields ever leave the browser; the application checks them again.
        let body = json!({
            "url": url,
            "referer": field("referer"),
            "cookies": field("cookies"),
            "userAgent": field("userAgent"),
            "fileName": field("fileName"),
        })
        .to_string();

        if self.running_version().is_none() {
            self.start()?;
        }
        match self.call("POST", "/add", Some(&body)) {
            Some(reply) if reply.get("ok").and_then(Value::as_bool) == Some(true) => Ok(()),
            Some(_) => Err("Rapid Download Manager refused the download".into()),
            None => Err("Rapid Download Manager is not running".into()),
        }
    }
}

/// Start `exe --minimized` so that it outlives this process and holds on to
/// none of its pipes (the browser is waiting for those to close).
fn spawn_detached(exe: &std::path::Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let command = || {
        let mut c = Command::new(exe);
        c.arg("--minimized").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        c
    };
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // SAFETY: the standard handles stay valid for the life of the process; clearing a flag on them is harmless.
        unsafe {
            SetHandleInformation(std::io::stdin().as_raw_handle() as _, HANDLE_FLAG_INHERIT, 0);
            SetHandleInformation(std::io::stdout().as_raw_handle() as _, HANDLE_FLAG_INHERIT, 0);
            SetHandleInformation(std::io::stderr().as_raw_handle() as _, HANDLE_FLAG_INHERIT, 0);
        }
        // Leave the browser's job object if it has one and permits it; otherwise start inside it.
        if command().creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB).spawn().is_ok() {
            return Ok(());
        }
        command().creation_flags(CREATE_NEW_PROCESS_GROUP).spawn().map(|_| ())
    }
    #[cfg(not(windows))]
    {
        command().spawn().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(json: &str) -> Vec<u8> {
        let mut out = (json.len() as u32).to_ne_bytes().to_vec();
        out.extend_from_slice(json.as_bytes());
        out
    }

    #[test]
    fn framing() {
        let mut input = std::io::Cursor::new([frame(r#"{"type":"status"}"#), frame(r#"{"a":1}"#)].concat());
        assert_eq!(read_message(&mut input).unwrap().unwrap()["type"], "status");
        assert_eq!(read_message(&mut input).unwrap().unwrap()["a"], 1);
        assert!(read_message(&mut input).unwrap().is_none(), "end of input is a normal end");

        let mut out = Vec::new();
        write_message(&mut out, &json!({ "ok": true })).unwrap();
        assert_eq!(out, frame(r#"{"ok":true}"#));
    }

    #[test]
    fn hostile_input_is_refused() {
        // A length far beyond the limit must not be allocated or read.
        let mut huge = std::io::Cursor::new(u32::MAX.to_ne_bytes().to_vec());
        assert!(read_message(&mut huge).is_err());
        let mut zero = std::io::Cursor::new(0u32.to_ne_bytes().to_vec());
        assert!(read_message(&mut zero).is_err());
        let mut not_json = std::io::Cursor::new(frame("hello"));
        assert!(read_message(&mut not_json).is_err());
        let mut cut_short = std::io::Cursor::new(frame(r#"{"type":"status"}"#)[..10].to_vec());
        assert!(read_message(&mut cut_short).is_err());
    }

    #[test]
    fn requests_are_checked_before_anything_is_forwarded() {
        // No application behind this folder: nothing can be reached, so only validation answers.
        let app = App { data_dir: std::env::temp_dir().join(format!("rdm-host-test-{}", std::process::id())) };
        for bad in [
            json!({ "type": "add" }),
            json!({ "type": "add", "url": "file:///C:/Windows/win.ini" }),
            json!({ "type": "add", "url": "javascript:alert(1)" }),
            json!({ "type": "add", "url": 5 }),
        ] {
            assert_eq!(handle(&app, &bad), json!({ "ok": false, "error": "not a web address" }), "{bad}");
        }
        assert_eq!(handle(&app, &json!({ "type": "cli", "args": ["/quit"] }))["ok"], false);
        assert_eq!(handle(&app, &json!("status"))["ok"], false);

        let status = handle(&app, &json!({ "type": "status" }));
        assert_eq!((&status["ok"], &status["running"]), (&json!(true), &json!(false)));
        assert!(status["captureExtensions"].as_array().is_some_and(|list| list.iter().any(|e| e == "zip")));
    }
}
