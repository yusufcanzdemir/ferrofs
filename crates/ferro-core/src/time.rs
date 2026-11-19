//! Local calendar dates as FILETIME ranges for `dm:` filters. Uses the Windows time zone
//! rules (including daylight saving), never the user's locale formatting.

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTimeAsFileTime};
use windows::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};

/// 100 ns ticks per day.
pub const DAY: u64 = 24 * 60 * 60 * 10_000_000;

fn to_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// Current time as FILETIME (UTC).
pub fn now() -> u64 {
    // SAFETY: no preconditions.
    to_u64(unsafe { GetSystemTimeAsFileTime() })
}

/// Today's local date as (year, month, day).
pub fn today() -> (u16, u16, u16) {
    // SAFETY: no preconditions.
    let t = unsafe { GetLocalTime() };
    (t.wYear, t.wMonth, t.wDay)
}

/// FILETIME (UTC) of local midnight at the start of the given date, or `None` if invalid.
pub fn local_midnight(year: u16, month: u16, day: u16) -> Option<u64> {
    if !(1601..=30827).contains(&year)
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
    {
        return None;
    }
    let local = SYSTEMTIME {
        wYear: year,
        wMonth: month,
        wDay: day,
        ..Default::default()
    };
    let mut utc = SYSTEMTIME::default();
    let mut ft = FILETIME::default();
    // SAFETY: all pointers reference live locals; None selects the current time zone.
    unsafe {
        TzSpecificLocalTimeToSystemTime(None, &local, &mut utc).ok()?;
        SystemTimeToFileTime(&utc, &mut ft).ok()?;
    }
    Some(to_u64(ft))
}

/// FILETIME of local midnight at the start of the day after the given date.
pub fn next_local_midnight(year: u16, month: u16, day: u16) -> Option<u64> {
    let (y, m, d) = if day < days_in_month(year, month) {
        (year, month, day + 1)
    } else if month < 12 {
        (year, month + 1, 1)
    } else {
        (year + 1, 1, 1)
    };
    local_midnight(y, m, d)
}

/// The calendar day before the given date.
pub fn previous_day(year: u16, month: u16, day: u16) -> (u16, u16, u16) {
    if day > 1 {
        (year, month, day - 1)
    } else if month > 1 {
        (year, month - 1, days_in_month(year, month - 1))
    } else {
        (year - 1, 12, 31)
    }
}

fn days_in_month(year: u16, month: u16) -> u16 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

/// Parses `YYYY-MM-DD`.
pub fn parse_date(text: &str) -> Option<(u16, u16, u16)> {
    let mut parts = text.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    local_midnight(year, month, day).map(|_| (year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_are_24_hours_apart_except_around_dst() {
        let a = local_midnight(2024, 6, 1).unwrap();
        let b = next_local_midnight(2024, 6, 1).unwrap();
        assert!((b - a).abs_diff(DAY) <= 3600 * 10_000_000);
        assert_eq!(
            next_local_midnight(2024, 12, 31),
            local_midnight(2025, 1, 1)
        );
        assert_eq!(next_local_midnight(2024, 2, 29), local_midnight(2024, 3, 1));
    }

    #[test]
    fn date_parsing() {
        assert_eq!(parse_date("2024-02-29"), Some((2024, 2, 29)));
        assert_eq!(parse_date("2023-02-29"), None);
        assert_eq!(parse_date("2024-13-01"), None);
        assert_eq!(parse_date("2024-01"), None);
        assert_eq!(parse_date("yesterday"), None);
    }

    #[test]
    fn now_is_today() {
        let (y, m, d) = today();
        let start = local_midnight(y, m, d).unwrap();
        let end = next_local_midnight(y, m, d).unwrap();
        assert!((start..end).contains(&now()));
    }
}
