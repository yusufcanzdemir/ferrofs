//! UTF-16 helpers for Win32 calls. All string conversions go through here.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
use windows::core::PWSTR;

/// A NUL-terminated UTF-16 string allocated by COM (`CoTaskMemAlloc`), freed on drop.
#[derive(Debug)]
pub struct CoTaskMemWide(PWSTR);

impl CoTaskMemWide {
    /// # Safety
    /// `ptr` must be null or a NUL-terminated string allocated with `CoTaskMemAlloc`
    /// that the caller owns.
    pub unsafe fn new(ptr: PWSTR) -> Self {
        Self(ptr)
    }

    pub fn to_path(&self) -> PathBuf {
        if self.0.is_null() {
            return PathBuf::new();
        }
        // SAFETY: non-null and NUL-terminated per the contract of `new`.
        let units = unsafe { self.0.as_wide() };
        PathBuf::from(OsString::from_wide(units))
    }
}

impl Drop for CoTaskMemWide {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated with CoTaskMemAlloc and owned by us (see `new`).
            unsafe { CoTaskMemFree(Some(self.0.0.cast_const().cast())) };
        }
    }
}

/// Copies `s` into a fixed-size UTF-16 buffer, truncating if needed and
/// always leaving a terminating NUL.
pub fn copy_into(dst: &mut [u16], s: &str) {
    let Some(max) = dst.len().checked_sub(1) else {
        return;
    };
    let mut n = 0;
    for unit in s.encode_utf16().take(max) {
        dst[n] = unit;
        n += 1;
    }
    dst[n] = 0;
}

/// Returns true if the window class of `hwnd` equals one of `names`.
/// Allocation-free so it can run inside the keyboard hook.
pub fn window_class_is(hwnd: HWND, names: &[&str]) -> bool {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is a valid writable buffer; the API never writes past its length.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    let Ok(len) = usize::try_from(len) else {
        return false;
    };
    let class = &buf[..len.min(buf.len())];
    names
        .iter()
        .any(|name| class.iter().copied().eq(name.encode_utf16()))
}

/// Window class name of `hwnd` for diagnostics (allocates).
pub fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is a valid writable buffer; the API never writes past its length.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    let len = usize::try_from(len).unwrap_or(0).min(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(test)]
mod tests {
    use super::copy_into;

    #[test]
    fn copy_into_truncates_and_terminates() {
        let mut buf = [0xFFFFu16; 4];
        copy_into(&mut buf, "abcdef");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }

    #[test]
    fn copy_into_handles_empty_buffer() {
        let mut buf: [u16; 0] = [];
        copy_into(&mut buf, "abc");
    }
}
