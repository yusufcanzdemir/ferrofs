//! Display formatting for sizes and FILETIME timestamps.

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows::Win32::System::Time::FileTimeToSystemTime;

/// `1.4 MB` style; empty for 0 (sizes are unknown until the raw MFT reader exists).
pub fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
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

/// Local `YYYY-MM-DD HH:MM`; empty for 0 (unknown).
pub fn format_time(filetime: u64) -> String {
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
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn times() {
        assert_eq!(format_time(0), "");
        // 2024-06-01 12:00 UTC; the local rendering depends on the time zone, so only
        // check the shape.
        let t = format_time(133_617_168_000_000_000);
        assert_eq!(t.len(), 16, "{t}");
        assert!(t.starts_with("2024-06-0"), "{t}");
    }
}
