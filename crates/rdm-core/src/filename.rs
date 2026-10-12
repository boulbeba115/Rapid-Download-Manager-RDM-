//! Deriving a safe local file name from what the server tells us.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

/// Extensions used while a download is in flight.
pub const PART_EXT: &str = "rdmpart";
pub const STATE_EXT: &str = "rdmstate";

/// Parse a `Content-Disposition` header (RFC 6266). `filename*` wins over `filename`.
pub fn from_content_disposition(header: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for param in split_params(header) {
        let Some((key, value)) = param.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "filename*" {
            // charset'lang'percent-encoded
            let mut parts = value.splitn(3, '\'');
            let charset = parts.next().unwrap_or("").to_ascii_lowercase();
            let _lang = parts.next();
            if let Some(enc) = parts.next() {
                let bytes: Vec<u8> = percent_decode_str(enc).collect();
                let decoded = if charset == "iso-8859-1" {
                    bytes.iter().map(|&b| b as char).collect()
                } else {
                    String::from_utf8_lossy(&bytes).into_owned()
                };
                extended = Some(decoded);
            }
        } else if key == "filename" {
            let v = unquote(value);
            // Browsers tolerate percent-encoded plain filenames; so do we.
            let decoded = percent_decode_str(&v).decode_utf8().map(|c| c.into_owned()).unwrap_or(v);
            plain = Some(decoded);
        }
    }
    extended.or(plain).map(|n| sanitize(&n)).filter(|n| !n.is_empty())
}

/// Split on `;` but not inside quoted strings.
fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
        } else if c == '\\' && quoted {
            cur.push(c);
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
            cur.push(c);
        } else if c == ';' && !quoted {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            // Like browsers: only `\"` and `\\` are escapes. Any other backslash
            // is kept so `sanitize` sees it as a path separator and drops the
            // directory part ("..\..\evil.exe" must not survive as a path).
            if c == '\\' && matches!(chars.peek(), Some('"') | Some('\\')) {
                out.push(chars.next().unwrap());
            } else {
                out.push(c);
            }
        }
        out
    } else {
        v.to_string()
    }
}

/// Last non-empty path segment of the URL, percent-decoded.
pub fn from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let seg = parsed.path_segments()?.rev().find(|s| !s.is_empty())?;
    let decoded = percent_decode_str(seg).decode_utf8_lossy().into_owned();
    let name = sanitize(&decoded);
    (!name.is_empty()).then_some(name)
}

/// Make a string safe to use as a Windows file name.
pub fn sanitize(name: &str) -> String {
    // Keep only the final component if the server sent a path.
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    let out = out.trim_start().to_string();
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut out = if RESERVED.contains(&stem.as_str()) { format!("_{out}") } else { out };
    // Leave room for " (123).rdmstate" within the 255-char component limit.
    if out.chars().count() > 200 {
        let ext = extension(&out).map(|e| format!(".{e}")).unwrap_or_default();
        let keep = 200usize.saturating_sub(ext.chars().count());
        out = out.chars().take(keep).collect::<String>() + &ext;
    }
    out
}

pub fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 10 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

pub fn ext_from_mime(mime: &str) -> Option<&'static str> {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Some(match mime.as_str() {
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/x-7z-compressed" => "7z",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/x-iso9660-image" => "iso",
        "application/pdf" => "pdf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/x-msdownload" | "application/vnd.microsoft.portable-executable" => "exe",
        "application/x-msi" => "msi",
        "application/vnd.android.package-archive" => "apk",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/css" => "css",
        "text/csv" => "csv",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/ogg" => "ogg",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "video/mp4" => "mp4",
        "video/x-matroska" => "mkv",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "video/x-msvideo" => "avi",
        "video/x-flv" => "flv",
        "video/mp2t" => "ts",
        _ => return None,
    })
}

/// Best file name for a response: header, then URL, then a generic fallback.
pub fn pick(content_disposition: Option<&str>, final_url: &str, content_type: Option<&str>) -> String {
    let mut name = content_disposition
        .and_then(from_content_disposition)
        .or_else(|| from_url(final_url))
        .unwrap_or_else(|| "download".to_string());
    if extension(&name).is_none() {
        if let Some(ext) = content_type.and_then(ext_from_mime) {
            name = format!("{name}.{ext}");
        }
    }
    name
}

pub fn part_path(final_path: &Path) -> PathBuf {
    append_ext(final_path, PART_EXT)
}

pub fn state_path(final_path: &Path) -> PathBuf {
    append_ext(final_path, STATE_EXT)
}

fn append_ext(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// `dir/name`, or `dir/name (n).ext` if the name (or its in-progress files) is taken.
pub fn unique_path(dir: &Path, name: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let free = |p: &Path| !p.exists() && !part_path(p).exists() && !state_path(p).exists() && !taken(p);
    let first = dir.join(name);
    if free(&first) {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && extension(name).is_some() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 1u32.. {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if free(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_variants() {
        assert_eq!(from_content_disposition(r#"attachment; filename="report.pdf""#).as_deref(), Some("report.pdf"));
        assert_eq!(from_content_disposition("attachment; filename=setup.exe").as_deref(), Some("setup.exe"));
        assert_eq!(
            from_content_disposition(r#"attachment; filename="fallback.txt"; filename*=UTF-8''na%C3%AFve%20file.txt"#)
                .as_deref(),
            Some("naïve file.txt")
        );
        assert_eq!(from_content_disposition(r#"attachment; filename="a;b.zip""#).as_deref(), Some("a;b.zip"));
        assert_eq!(from_content_disposition(r#"inline; filename="..\..\evil.exe""#).as_deref(), Some("evil.exe"));
        assert_eq!(from_content_disposition("attachment").as_deref(), None);
    }

    #[test]
    fn url_names() {
        assert_eq!(from_url("https://h.com/a/b/file%20name.zip?x=1#f").as_deref(), Some("file name.zip"));
        assert_eq!(from_url("https://h.com/dir/").as_deref(), Some("dir"));
        assert_eq!(from_url("https://h.com/").as_deref(), None);
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("a<b>c:d.txt"), "a_b_c_d.txt");
        assert_eq!(sanitize("name. . "), "name");
        assert_eq!(sanitize("CON.txt"), "_CON.txt");
        assert_eq!(sanitize("dir/sub\\x.bin"), "x.bin");
    }

    #[test]
    fn picking() {
        assert_eq!(pick(None, "https://h.com/get?id=5", Some("application/pdf")), "get.pdf");
        assert_eq!(pick(None, "https://h.com/", None), "download");
        assert_eq!(pick(Some("attachment; filename=x.iso"), "https://h.com/y.bin", None), "x.iso");
    }

    #[test]
    fn unique_names() {
        let dir = Path::new("Z:\\does-not-exist");
        let taken = |p: &Path| p.ends_with("a.zip") || p.ends_with("a (1).zip");
        assert_eq!(unique_path(dir, "a.zip", taken), dir.join("a (2).zip"));
        assert_eq!(unique_path(dir, "b.zip", |_| false), dir.join("b.zip"));
    }
}
