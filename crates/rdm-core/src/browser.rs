//! Browser integration: registering `rdm-host` as a native-messaging host.
//!
//! Native messaging is the browser's own mechanism for letting one specific
//! extension talk to one specific program: the browser reads a small manifest
//! (found through the registry), starts the program it names, and connects it
//! to the extension over a pipe — but only if the extension's ID is listed in
//! `allowed_origins`. Web pages and other extensions cannot reach the host.
//!
//! Everything here is per-user (`HKEY_CURRENT_USER`); no administrator rights.

use std::path::{Path, PathBuf};

/// Name the extension asks the browser for. Must match `extension/background.js`.
pub const HOST_NAME: &str = "io.github.boulbeba115.rdm";
/// The executable the browser starts, next to `rdm.exe`.
pub const HOST_EXE: &str = "rdm-host.exe";
/// The host's manifest, written next to the executables when registering.
pub const MANIFEST_FILE: &str = "rdm-host.json";

/// ID of the extension in the `extension` folder. It follows from the `key`
/// in `extension/manifest.json`, so it is the same on every computer.
pub const BUNDLED_EXTENSION_ID: &str = "pfdfjjjecmhdcmoedibbhgajnogkiojg";

/// IDs the web stores assign once the extension is published there. A store
/// signs with its own key, so these differ from [`BUNDLED_EXTENSION_ID`].
/// Setting one does two things: that extension may talk to the host, and
/// registering makes the browser offer to install it from its store — the
/// only way Chrome and Edge on Windows let an installer add an extension.
pub const CHROME_STORE_ID: Option<&str> = None;
pub const EDGE_STORE_ID: Option<&str> = None;

const CHROME_UPDATE_URL: &str = "https://clients2.google.com/service/update2/crx";
const EDGE_UPDATE_URL: &str = "https://edge.microsoft.com/extensionwebstorebase/v1/crx";

/// Chromium-family browsers and their registry key below `Software`.
const BROWSERS: [(&str, &str); 4] = [
    ("Google Chrome", r"Google\Chrome"),
    ("Microsoft Edge", r"Microsoft\Edge"),
    ("Chromium", "Chromium"),
    ("Brave", r"BraveSoftware\Brave-Browser"),
];

/// Every extension that may use the host.
pub fn extension_ids() -> Vec<&'static str> {
    let mut ids = vec![BUNDLED_EXTENSION_ID];
    ids.extend(CHROME_STORE_ID);
    ids.extend(EDGE_STORE_ID);
    ids.dedup();
    ids
}

/// The browser passes the caller's origin (`chrome-extension://<id>/`) as the
/// host's first argument. It has already checked it against the manifest;
/// the host checks again rather than trust how it was started.
pub fn is_allowed_origin(origin: &str) -> bool {
    origin
        .strip_prefix("chrome-extension://")
        .map(|rest| rest.trim_end_matches('/'))
        .is_some_and(|id| extension_ids().contains(&id))
}

pub fn manifest_json(host_exe: &Path) -> String {
    let origins: Vec<String> = extension_ids().iter().map(|id| format!("chrome-extension://{id}/")).collect();
    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": "Rapid Download Manager browser integration",
        "path": host_exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": origins,
    });
    serde_json::to_string_pretty(&manifest).expect("a JSON value always serializes")
}

fn host_key(root: &str, browser_key: &str) -> String {
    format!(r"{root}\{browser_key}\NativeMessagingHosts\{HOST_NAME}")
}

fn store_keys(root: &str) -> Vec<(String, &'static str)> {
    let mut keys = Vec::new();
    if let Some(id) = CHROME_STORE_ID {
        keys.push((format!(r"{root}\Google\Chrome\Extensions\{id}"), CHROME_UPDATE_URL));
    }
    if let Some(id) = EDGE_STORE_ID {
        keys.push((format!(r"{root}\Microsoft\Edge\Extensions\{id}"), EDGE_UPDATE_URL));
    }
    keys
}

/// Make the host in `dir` (the folder holding `rdm-host.exe`) known to the
/// browsers. Returns the manifest that was written.
pub fn register(dir: &Path) -> std::io::Result<PathBuf> {
    register_under("Software", dir)
}

/// Remove what [`register`] wrote. Not having been registered is not an error.
pub fn unregister(dir: &Path) -> std::io::Result<()> {
    unregister_under("Software", dir)
}

/// The executable the browsers currently start for the extension, if any.
pub fn registered_host() -> Option<PathBuf> {
    registered_host_under("Software")
}

fn register_under(root: &str, dir: &Path) -> std::io::Result<PathBuf> {
    let host = dir.join(HOST_EXE);
    if !host.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{} is missing", host.display())));
    }
    let manifest = dir.join(MANIFEST_FILE);
    std::fs::write(&manifest, manifest_json(&host))?;
    let manifest_text = manifest.to_string_lossy().into_owned();
    for (_, browser) in BROWSERS {
        registry::set_string(&host_key(root, browser), "", &manifest_text)?;
    }
    for (key, update_url) in store_keys(root) {
        registry::set_string(&key, "update_url", update_url)?;
    }
    Ok(manifest)
}

fn unregister_under(root: &str, dir: &Path) -> std::io::Result<()> {
    for (_, browser) in BROWSERS {
        registry::delete_key(&host_key(root, browser))?;
    }
    for (key, _) in store_keys(root) {
        registry::delete_key(&key)?;
    }
    match std::fs::remove_file(dir.join(MANIFEST_FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn registered_host_under(root: &str) -> Option<PathBuf> {
    BROWSERS.iter().find_map(|(_, browser)| {
        let manifest = registry::get_string(&host_key(root, browser), "")?;
        let json: serde_json::Value = crate::state::load_json(Path::new(&manifest))?;
        let host = PathBuf::from(json.get("path")?.as_str()?);
        host.is_file().then_some(host)
    })
}

#[cfg(windows)]
mod registry {
    use std::io::{Error, Result};

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE,
        REG_SZ, RRF_RT_REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn check(code: u32) -> Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(Error::from_raw_os_error(code as i32))
        }
    }

    /// Set a string value (`name` empty: the key's default value), creating the key.
    pub fn set_string(key: &str, name: &str, value: &str) -> Result<()> {
        let (key, name, value) = (wide(key), wide(name), wide(value));
        let mut handle: HKEY = std::ptr::null_mut();
        // SAFETY: all pointers refer to live, NUL-terminated buffers; the handle is closed below.
        unsafe {
            check(RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut handle,
                std::ptr::null_mut(),
            ))?;
            let result = check(RegSetValueExW(handle, name.as_ptr(), 0, REG_SZ, value.as_ptr() as *const u8, (value.len() * 2) as u32));
            RegCloseKey(handle);
            result
        }
    }

    pub fn get_string(key: &str, name: &str) -> Option<String> {
        let (key, name) = (wide(key), wide(name));
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `bytes` tells the system how large `buf` is; RRF_RT_REG_SZ guarantees a terminated string.
        let code = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut bytes)
        };
        if code != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    pub fn delete_key(key: &str) -> Result<()> {
        let key = wide(key);
        // SAFETY: `key` is a live, NUL-terminated buffer.
        match unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => Ok(()),
            code => check(code),
        }
    }
}

#[cfg(not(windows))]
mod registry {
    use std::io::{Error, ErrorKind, Result};

    pub fn set_string(_key: &str, _name: &str, _value: &str) -> Result<()> {
        Err(Error::new(ErrorKind::Unsupported, "browser registration is only implemented for Windows"))
    }

    pub fn get_string(_key: &str, _name: &str) -> Option<String> {
        None
    }

    pub fn delete_key(_key: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert!(is_allowed_origin(&format!("chrome-extension://{BUNDLED_EXTENSION_ID}/")));
        assert!(is_allowed_origin(&format!("chrome-extension://{BUNDLED_EXTENSION_ID}")));
        assert!(!is_allowed_origin("chrome-extension://abcdefghijklmnopabcdefghijklmnop/"));
        assert!(!is_allowed_origin(&format!("https://{BUNDLED_EXTENSION_ID}/")));
        assert!(!is_allowed_origin(""));
    }

    /// The ID is not arbitrary: it is the hash of the public key in the
    /// extension's manifest. If someone changes one without the other, the
    /// browser would refuse the connection — catch that here instead.
    #[test]
    fn bundled_id_matches_the_extension_key() {
        use base64::Engine;
        use sha2::Digest;
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../extension/manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let key = base64::engine::general_purpose::STANDARD.decode(manifest["key"].as_str().unwrap()).unwrap();
        let id: String = sha2::Sha256::digest(&key)[..16].iter().flat_map(|b| [b >> 4, b & 15]).map(|n| (b'a' + n) as char).collect();
        assert_eq!(id, BUNDLED_EXTENSION_ID);

        let background = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../extension/background.js")).unwrap();
        assert!(background.contains(&format!("\"{HOST_NAME}\"")), "background.js must ask for the host by the registered name");
    }

    #[test]
    fn manifest_lists_only_our_extensions() {
        let json: serde_json::Value = serde_json::from_str(&manifest_json(Path::new(r"C:\Apps\RDM\rdm-host.exe"))).unwrap();
        assert_eq!(json["name"], HOST_NAME);
        assert_eq!(json["type"], "stdio");
        assert_eq!(json["path"], r"C:\Apps\RDM\rdm-host.exe");
        let origins = json["allowed_origins"].as_array().unwrap();
        assert_eq!(origins.len(), extension_ids().len());
        assert!(origins.iter().all(|o| o.as_str().unwrap().starts_with("chrome-extension://") && !o.as_str().unwrap().contains('*')));
    }

    #[cfg(windows)]
    #[test]
    fn register_and_unregister() {
        // A scratch registry root, so the real registration is never touched.
        let root = format!(r"Software\RdmTest-{}", std::process::id());
        let dir = std::env::temp_dir().join(format!("rdm-browser-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(register_under(&root, &dir).is_err(), "nothing to register without the host executable");
        std::fs::write(dir.join(HOST_EXE), b"").unwrap();
        let manifest = register_under(&root, &dir).unwrap();
        assert_eq!(manifest, dir.join(MANIFEST_FILE));
        for (_, browser) in BROWSERS {
            assert_eq!(registry::get_string(&host_key(&root, browser), "").as_deref(), Some(&*manifest.to_string_lossy()));
        }
        assert_eq!(registered_host_under(&root), Some(dir.join(HOST_EXE)));

        unregister_under(&root, &dir).unwrap();
        assert_eq!(registered_host_under(&root), None);
        assert!(!manifest.exists());
        unregister_under(&root, &dir).unwrap(); // twice is fine
        registry::delete_key(&root).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
