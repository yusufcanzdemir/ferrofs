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
    if filetime == 0 {
        return String::new();
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
    if !ok {
        return String::new();
    }
    format!("{:04}-{:02}-{:02}", time.wYear, time.wMonth, time.wDay)
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
        assert!((share(1, 4) - 0.25).abs() < 1e-6);
        assert!(share(1, 0).abs() < f32::EPSILON);
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "");
        let d = date(133_617_168_000_000_000); // 2024-06-01 12:00 UTC
        assert!(d.starts_with("2024-06-0"), "{d}");
    }
}
