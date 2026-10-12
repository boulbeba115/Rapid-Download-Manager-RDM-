//! Formatting helpers shared by the GUI and the CLI.

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ if value >= 100.0 => format!("{value:.0} {}", UNITS[unit]),
        _ if value >= 10.0 => format!("{value:.1} {}", UNITS[unit]),
        _ => format!("{value:.2} {}", UNITS[unit]),
    }
}

pub fn format_speed(bytes_per_sec: u64) -> String {
    format!("{}/s", format_size(bytes_per_sec))
}

pub fn format_duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60, secs % 60);
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m} min")
    } else if m > 0 {
        format!("{m} min {s} sec")
    } else {
        format!("{s} sec")
    }
}

/// Seconds left at the current speed; `None` when it cannot be estimated.
pub fn eta_secs(size: Option<u64>, downloaded: u64, speed: u64) -> Option<u64> {
    let size = size?;
    if speed == 0 || downloaded >= size {
        return None;
    }
    Some((size - downloaded).div_ceil(speed))
}

/// Parse "500k", "2.5M", "1g", "1048576" (bytes per second or sizes).
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim().to_ascii_lowercase();
    let t = t.trim_end_matches("/s").trim_end_matches("ib").trim_end_matches('b').trim();
    let (num, mult) = match t.chars().last()? {
        'k' => (&t[..t.len() - 1], 1024f64),
        'm' => (&t[..t.len() - 1], 1024f64 * 1024.0),
        'g' => (&t[..t.len() - 1], 1024f64 * 1024.0 * 1024.0),
        _ => (t, 1.0),
    };
    let value: f64 = num.trim().parse().ok()?;
    (value >= 0.0 && value.is_finite()).then_some((value * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.50 KB");
        assert_eq!(format_size(52 * 1024 * 1024), "52.0 MB");
        assert_eq!(format_size(4_400_000_000), "4.10 GB");
        assert_eq!(parse_size("500k"), Some(512_000));
        assert_eq!(parse_size("2.5M"), Some(2_621_440));
        assert_eq!(parse_size("1 MB/s"), Some(1_048_576));
        assert_eq!(parse_size("1048576"), Some(1_048_576));
        assert_eq!(parse_size("abc"), None);
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(59), "59 sec");
        assert_eq!(format_duration(130), "2 min 10 sec");
        assert_eq!(format_duration(3700), "1 h 1 min");
        assert_eq!(eta_secs(Some(1000), 0, 100), Some(10));
        assert_eq!(eta_secs(Some(1000), 0, 0), None);
        assert_eq!(eta_secs(None, 0, 100), None);
    }
}

/// `https://h/part*.zip`, 1..=3, 2 digits → `part01.zip`, `part02.zip`, `part03.zip`.
/// Only the first `*` is replaced. At most `limit` addresses are produced.
pub fn expand_template(template: &str, from: u32, to: u32, digits: usize, limit: usize) -> Vec<String> {
    if !template.contains('*') || to < from {
        return Vec::new();
    }
    (from..=to).take(limit).map(|n| template.replacen('*', &format!("{n:0digits$}"), 1)).collect()
}

/// If `text` is nothing but one http(s) link to a file whose extension is in
/// `extensions`, return that link. Used for clipboard monitoring.
pub fn download_link(text: &str, extensions: &[String]) -> Option<String> {
    let url = text.trim();
    if url.len() > 2048 || url.contains(char::is_whitespace) || !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let ext = crate::filename::from_url(url).and_then(|n| crate::filename::extension(&n))?;
    extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext)).then(|| url.to_string())
}

#[cfg(test)]
mod more_tests {
    use super::*;

    #[test]
    fn templates() {
        assert_eq!(
            expand_template("https://h/part*.zip", 9, 11, 3, 100),
            vec!["https://h/part009.zip", "https://h/part010.zip", "https://h/part011.zip"]
        );
        assert_eq!(expand_template("https://h/a*b*.bin", 1, 1, 1, 100), vec!["https://h/a1b*.bin"]);
        assert!(expand_template("https://h/none.zip", 1, 5, 2, 100).is_empty());
        assert!(expand_template("https://h/x*.zip", 5, 1, 2, 100).is_empty());
        assert_eq!(expand_template("https://h/x*.zip", 0, 99_999, 1, 50).len(), 50);
    }

    #[test]
    fn clipboard_links() {
        let exts: Vec<String> = ["zip", "mp4"].iter().map(|s| s.to_string()).collect();
        assert_eq!(download_link("  https://h.com/a/File.ZIP?x=1 \n", &exts).as_deref(), Some("https://h.com/a/File.ZIP?x=1"));
        assert_eq!(download_link("https://h.com/page.html", &exts), None);
        assert_eq!(download_link("https://h.com/", &exts), None);
        assert_eq!(download_link("see https://h.com/a.zip", &exts), None, "prose is not a link");
        assert_eq!(download_link("ftp://h.com/a.zip", &exts), None);
        assert_eq!(download_link("C:/Users/me/a.zip", &exts), None);
    }
}
