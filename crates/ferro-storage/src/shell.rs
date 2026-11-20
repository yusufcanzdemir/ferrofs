//! Shell actions: open a file or folder, reveal it in Explorer.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use anyhow::{Context, Result, bail};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

fn wide_nul(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain([0]).collect()
}

/// Opens the file or folder with its default handler.
pub fn open(path: &str) -> Result<()> {
    let file = wide_nul(path);
    // SAFETY: strings are NUL-terminated and outlive the call.
    let code = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(file.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // Values <= 32 are errors.
    if code.0 as isize <= 32 {
        bail!(
            "cannot open {path} (ShellExecute error {})",
            code.0 as isize
        );
    }
    Ok(())
}

/// Opens the containing folder in Explorer with the item selected.
pub fn reveal(path: &str) -> Result<()> {
    let file = wide_nul(path);
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: `file` is NUL-terminated; on success we own `pidl` and free it below.
    unsafe { SHParseDisplayName(PCWSTR(file.as_ptr()), None, &mut pidl, 0, None) }
        .with_context(|| format!("cannot resolve {path}"))?;
    // SAFETY: `pidl` is a valid absolute PIDL; with no child list the item itself is selected.
    let result = unsafe { SHOpenFolderAndSelectItems(pidl, None, 0) };
    // SAFETY: allocated by SHParseDisplayName with the COM allocator.
    unsafe { CoTaskMemFree(Some(pidl.cast_const().cast())) };
    result.context("cannot open Explorer")
}
