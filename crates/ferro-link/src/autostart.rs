//! "Start with Windows" via the per-user `Run` registry key (SPEC §1.1).

use std::ffi::OsStr;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::core::PCWSTR;

use crate::wide::to_wide_nul;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "ferrofs-link";

/// Whether ferro-link is registered to start with Windows.
pub fn is_enabled() -> bool {
    value_exists(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME)
}

/// Registers (with the current executable path) or unregisters autostart.
pub fn set_enabled(enabled: bool) -> Result<()> {
    if enabled {
        let exe = std::env::current_exe().context("cannot locate ferro-link.exe")?;
        let command = format!("\"{}\"", exe.display());
        set_value(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME, &command)
    } else {
        delete_value(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME)
    }
}

fn value_exists(root: HKEY, subkey: &str, name: &str) -> bool {
    let subkey = to_wide_nul(OsStr::new(subkey));
    let name = to_wide_nul(OsStr::new(name));
    let mut size = 0u32;
    // SAFETY: strings are NUL-terminated and outlive the call; only the size is queried.
    let status = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS
}

fn set_value(root: HKEY, subkey: &str, name: &str, data: &str) -> Result<()> {
    let subkey = to_wide_nul(OsStr::new(subkey));
    let name = to_wide_nul(OsStr::new(name));
    let data = to_wide_nul(OsStr::new(data));
    let bytes = u32::try_from(data.len() * 2)?;
    // SAFETY: all buffers are NUL-terminated and outlive the call; `bytes` includes the NUL.
    let status = unsafe {
        RegSetKeyValueW(
            root,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            bytes,
        )
    };
    check(status).context("cannot write the autostart registry value")
}

fn delete_value(root: HKEY, subkey: &str, name: &str) -> Result<()> {
    let subkey = to_wide_nul(OsStr::new(subkey));
    let name = to_wide_nul(OsStr::new(name));
    // SAFETY: strings are NUL-terminated and outlive the call.
    let status =
        unsafe { RegDeleteKeyValueW(root, PCWSTR(subkey.as_ptr()), PCWSTR(name.as_ptr())) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(()); // already disabled
    }
    check(status).context("cannot delete the autostart registry value")
}

fn check(status: WIN32_ERROR) -> windows::core::Result<()> {
    status.ok()
}

#[cfg(test)]
mod tests {
    use windows::Win32::System::Registry::RegDeleteTreeW;

    use super::*;

    /// Uses a throwaway key under HKCU\Software instead of the real Run key.
    #[test]
    fn set_query_delete_roundtrip() {
        let subkey = format!(r"Software\ferrofs-tests\autostart-{}", std::process::id());
        let root = HKEY_CURRENT_USER;
        assert!(!value_exists(root, &subkey, VALUE_NAME));
        set_value(root, &subkey, VALUE_NAME, r#""C:\x\ferro-link.exe""#).unwrap();
        assert!(value_exists(root, &subkey, VALUE_NAME));
        delete_value(root, &subkey, VALUE_NAME).unwrap();
        assert!(!value_exists(root, &subkey, VALUE_NAME));
        delete_value(root, &subkey, VALUE_NAME).unwrap(); // idempotent

        let tests_key = to_wide_nul(OsStr::new(r"Software\ferrofs-tests"));
        // SAFETY: NUL-terminated key name; removes only the test key tree.
        let _ = unsafe { RegDeleteTreeW(root, PCWSTR(tests_key.as_ptr())) };
    }
}
