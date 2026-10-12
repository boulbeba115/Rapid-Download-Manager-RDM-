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
