//! Small security primitives shared across the engine: random tokens,
//! constant-time comparison, secrets at rest, the mark of the web, and the
//! rule for which hosts a download's cookies and passwords may be sent to.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

/// A fresh 256-bit token from the operating system's generator, as hex.
pub fn random_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(hex::encode(bytes))
}

/// Compare without leaking, through timing, how many leading bytes matched.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// An address fit for a log file: no credentials, and no query string (which
/// is where signed download links keep their access tokens).
pub fn redact_url(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut u) => {
            let had_query = u.query().is_some();
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            if had_query {
                format!("{u}?…")
            } else {
                u.to_string()
            }
        }
        Err(_) => "(unparsable address)".into(),
    }
}

// ---------------------------------------------------------------- Credential scope

/// The part of a host name that one party registers. Without the public
/// suffix list this is a guess, erring on the narrow side: `a.example.com` →
/// `example.com`, `a.example.co.uk` → `example.co.uk`.
fn site_of(host: &str) -> &str {
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return host;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    let (tld, second) = (labels[labels.len() - 1], labels[labels.len() - 2]);
    // `co.uk`, `com.au`, `ac.jp` …: a short label under a country code is a public suffix.
    let keep = if tld.len() == 2 && second.len() <= 3 { 3 } else { 2 };
    let start: usize = labels[..labels.len() - keep].iter().map(|l| l.len() + 1).sum();
    &host[start..]
}

/// What may accompany a request to `target` when the credentials were given
/// for `origin`: `(cookies, password)`.
///
/// A redirect or a playlist can point anywhere, including at a server that
/// only wants the session cookie. Cookies therefore stay within the site they
/// came from, passwords within the exact host and port, and neither is sent
/// over plain HTTP if it was given for HTTPS.
pub fn credential_scope(origin: &str, target: &str) -> (bool, bool) {
    if origin == target {
        return (true, true);
    }
    let (Ok(a), Ok(b)) = (Url::parse(origin), Url::parse(target)) else { return (false, false) };
    if a.scheme() == "https" && b.scheme() != "https" {
        return (false, false);
    }
    let (Some(ha), Some(hb)) = (a.host_str(), b.host_str()) else { return (false, false) };
    let (ha, hb) = (ha.to_ascii_lowercase(), hb.to_ascii_lowercase());
    let same_host = ha == hb;
    let same_origin = same_host && a.port_or_known_default() == b.port_or_known_default();
    (same_host || site_of(&ha) == site_of(&hb), same_origin)
}

// ---------------------------------------------------------------- Mark of the web

/// Tag a finished file as "came from the Internet" (the `Zone.Identifier`
/// stream browsers write), so SmartScreen, Office Protected View and the
/// "this file came from another computer" prompt apply to it exactly as they
/// would had the browser saved it. Does nothing on file systems without
/// alternate data streams.
pub fn mark_of_the_web(path: &Path, url: &str) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_os_string();
        stream.push(":Zone.Identifier");
        let mut text = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
        if let Ok(mut u) = Url::parse(url) {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_fragment(None);
            text.push_str(&format!("HostUrl={u}\r\n"));
        }
        let _ = std::fs::write(stream, text);
    }
    #[cfg(not(windows))]
    {
        let _ = (path, url);
    }
}

// ---------------------------------------------------------------- Secrets at rest

static PROTECT_AT_REST: AtomicBool = AtomicBool::new(true);
const PROTECTED_PREFIX: &str = "dpapi:";

/// Whether passwords and cookies are encrypted when they are written to the
/// settings files (Windows data protection, tied to the user account). A
/// portable copy turns this off so its settings keep working on another PC.
pub fn set_protect_at_rest(on: bool) {
    PROTECT_AT_REST.store(on, Ordering::Relaxed);
}

/// The form of `secret` that goes into a file.
pub fn protect(secret: &str) -> String {
    if secret.is_empty() || !PROTECT_AT_REST.load(Ordering::Relaxed) {
        return secret.to_string();
    }
    match dpapi::protect(secret.as_bytes()) {
        Some(blob) => {
            use base64::Engine;
            format!("{PROTECTED_PREFIX}{}", base64::engine::general_purpose::STANDARD.encode(blob))
        }
        None => secret.to_string(),
    }
}

/// Undo [`protect`]. Text that was never protected (files written by an older
/// version, or edited by hand) is returned as it is.
pub fn reveal(stored: &str) -> String {
    let Some(encoded) = stored.strip_prefix(PROTECTED_PREFIX) else { return stored.to_string() };
    use base64::Engine;
    let clear = base64::engine::general_purpose::STANDARD.decode(encoded).ok().and_then(|blob| dpapi::unprotect(&blob));
    match clear.and_then(|bytes| String::from_utf8(bytes).ok()) {
        Some(text) => text,
        None => {
            log::warn!("a saved password or cookie could not be decrypted (settings copied from another account?); it was dropped");
            String::new()
        }
    }
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

    fn run(data: &[u8], encrypt: bool) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: `input` points at `data` for the duration of the call; on success the
        // system allocates `output`, which is copied and then released with LocalFree.
        unsafe {
            let ok = if encrypt {
                CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
            } else {
                CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
            };
            if ok == 0 || output.pbData.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            LocalFree(output.pbData as _);
            Some(bytes)
        }
    }

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, true)
    }

    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, false)
    }
}

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(_data: &[u8]) -> Option<Vec<u8>> {
        None
    }

    pub fn unprotect(_data: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

/// `#[serde(with = "security::secret")]` for a `String` field.
pub mod secret {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &str, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::protect(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        Ok(super::reveal(&String::deserialize(d)?))
    }
}

/// `#[serde(with = "security::secret_opt")]` for an `Option<String>` field.
pub mod secret_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => s.serialize_some(&super::protect(v)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.map(|v| super::reveal(&v)))
    }
}

/// `#[serde(with = "security::secret_pair")]` for `Option<(user, password)>`: only the password is protected.
pub mod secret_pair {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<(String, String)>, s: S) -> Result<S::Ok, S::Error> {
        value.as_ref().map(|(user, pass)| (user.clone(), super::protect(pass))).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<(String, String)>, D::Error> {
        Ok(Option::<(String, String)>::deserialize(d)?.map(|(user, pass)| (user, super::reveal(&pass))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_comparison() {
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(constant_time_eq(a.as_bytes(), a.as_bytes()));
        assert!(!constant_time_eq(a.as_bytes(), b.as_bytes()));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn redaction() {
        assert_eq!(redact_url("https://user:pw@h.com/a/b.zip?sig=SECRET#x"), "https://h.com/a/b.zip?…");
        assert_eq!(redact_url("http://h.com/a.bin"), "http://h.com/a.bin");
    }

    #[test]
    fn credentials_stay_with_their_site() {
        let scope = |a, b| credential_scope(a, b);
        assert_eq!(scope("https://a.com/x", "https://a.com/x"), (true, true));
        assert_eq!(scope("https://a.com/x", "https://a.com/other"), (true, true));
        // A sibling host of the same site may see the cookies, never the password.
        assert_eq!(scope("https://www.host.com/get", "https://dl3.host.com/file"), (true, false));
        assert_eq!(scope("https://www.host.co.uk/get", "https://cdn.host.co.uk/file"), (true, false));
        // Another site sees neither.
        assert_eq!(scope("https://a.com/x", "https://evil.example/x"), (false, false));
        assert_eq!(scope("https://one.co.uk/x", "https://two.co.uk/x"), (false, false));
        assert_eq!(scope("http://10.0.0.1/x", "http://10.0.0.2/x"), (false, false));
        // Same host, other port: cookies are not port-bound, passwords are.
        assert_eq!(scope("http://127.0.0.1:8000/x", "http://127.0.0.1:9000/x"), (true, false));
        // Never downgraded to plain HTTP.
        assert_eq!(scope("https://a.com/x", "http://a.com/x"), (false, false));
        assert_eq!(scope("https://a.com/x", "not a url"), (false, false));
    }

    #[test]
    fn secrets_round_trip() {
        let stored = protect("hunter2");
        if cfg!(windows) {
            assert!(stored.starts_with(PROTECTED_PREFIX), "{stored}");
            assert!(!stored.contains("hunter2"));
        }
        assert_eq!(reveal(&stored), "hunter2");
        // Plain text from an older settings file is accepted as it is.
        assert_eq!(reveal("plain"), "plain");
        assert_eq!(protect(""), "");
        // Damaged data is dropped, not passed on as a password.
        assert_eq!(reveal("dpapi:AAAA"), "");
    }

    #[cfg(windows)]
    #[test]
    fn finished_files_are_marked() {
        let dir = std::env::temp_dir().join(format!("rdm-motw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("setup.exe");
        std::fs::write(&file, b"MZ").unwrap();
        mark_of_the_web(&file, "https://user:pw@h.com/setup.exe#frag");
        let mut stream = file.clone().into_os_string();
        stream.push(":Zone.Identifier");
        let zone = std::fs::read_to_string(stream).unwrap();
        assert!(zone.contains("ZoneId=3") && zone.contains("HostUrl=https://h.com/setup.exe\r\n"), "{zone}");
        assert_eq!(std::fs::read(&file).unwrap(), b"MZ");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
