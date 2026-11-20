//! Display formatting for sizes, counts and FILETIME timestamps.

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows::Win32::System::Time::FileTimeToSystemTime;

/// `1.4 GB` style (1024-based).
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    // Display only; precision loss above 2^53 bytes is irrelevant.
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Parses `64 KB`, `1.5mb`, `300` (bytes); 1024-based units.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let number: f64 = text[..split].trim().parse().ok()?;
    let unit: u64 = match text[split..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return None,
    };
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    // Bounded by the checks above; rounding to whole bytes is intended.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let bytes = (number * unit as f64).round() as u64;
    Some(bytes)
}

/// `1,234,567`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1 file`, `1,234 files` (`plural` = `None` adds an `s`).
pub fn counted(n: u64, singular: &str, plural: Option<&str>) -> String {
    let word = match (n, plural) {
        (1, _) => singular.to_owned(),
        (_, Some(plural)) => plural.to_owned(),
        (_, None) => format!("{singular}s"),
    };
    format!("{} {word}", count(n))
}

/// `part` as a share of `whole`, 0.0..=1.0.
pub fn share(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        return 0.0;
    }
    // Display only.
    #[allow(clippy::cast_precision_loss)]
    let ratio = part as f64 / whole as f64;
    #[allow(clippy::cast_possible_truncation)]
    let ratio = ratio.clamp(0.0, 1.0) as f32;
    ratio
}

/// Local `YYYY-MM-DD`; empty for 0 (unknown).
pub fn date(filetime: u64) -> String {
    local_time(filetime).map_or_else(String::new, |t| {
        format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay)
    })
}

/// Local `YYYY-MM-DD HH:MM`; empty for 0 (unknown).
pub fn date_time(filetime: u64) -> String {
    local_time(filetime).map_or_else(String::new, |t| {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute
        )
    })
}

fn local_time(filetime: u64) -> Option<SYSTEMTIME> {
    if filetime == 0 {
        return None;
    }
    let utc = FILETIME {
        // Splitting a u64 into its two halves.
        #[allow(clippy::cast_possible_truncation)]
        dwLowDateTime: filetime as u32,
        dwHighDateTime: (filetime >> 32) as u32,
    };
    let mut local = FILETIME::default();
    let mut time = SYSTEMTIME::default();
    // SAFETY: all pointers reference live locals.
    let ok = unsafe {
        FileTimeToLocalFileTime(&utc, &mut local).is_ok()
            && FileTimeToSystemTime(&local, &mut time).is_ok()
    };
    ok.then_some(time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_counts_shares() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(5 << 30), "5.0 GB");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(count(12), "12");
        assert_eq!(counted(1, "file", None), "1 file");
        assert_eq!(counted(1200, "file", None), "1,200 files");
        assert_eq!(counted(2, "copy", Some("copies")), "2 copies");
        assert!((share(1, 4) - 0.25).abs() < 1e-6);
        assert!(share(1, 0).abs() < f32::EPSILON);
    }

    #[test]
    fn parsing_sizes() {
        assert_eq!(parse_size("64 KB"), Some(65_536));
        assert_eq!(parse_size("1.5mb"), Some(1_572_864));
        assert_eq!(parse_size("300"), Some(300));
        assert_eq!(parse_size(" 2 G "), Some(2 << 30));
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("-1 kb"), None);
        assert_eq!(parse_size("5 xb"), None);
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "");
        let d = date(133_617_168_000_000_000); // 2024-06-01 12:00 UTC
        assert!(d.starts_with("2024-06-0"), "{d}");
        assert_eq!(date_time(133_617_168_000_000_000).len(), 16);
    }
}
