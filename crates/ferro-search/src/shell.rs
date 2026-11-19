//! Shell actions on a result: open, reveal in Explorer, copy the file to the clipboard.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DROPFILES, SHOpenFolderAndSelectItems, SHParseDisplayName, ShellExecuteW,
};
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

/// Puts the file itself on the clipboard (`CF_HDROP`), so it can be pasted in Explorer.
pub fn copy_file(owner: Option<isize>, path: &str) -> Result<()> {
    let header = size_of::<DROPFILES>();
    // Path, its NUL and the list-terminating NUL.
    let mut wide: Vec<u16> = OsStr::new(path).encode_wide().collect();
    wide.extend([0, 0]);
    let bytes = header + wide.len() * 2;

    // SAFETY: plain movable global allocation; ownership passes to the clipboard on success.
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.context("GlobalAlloc failed")?;
    // SAFETY: `memory` is a fresh allocation of `bytes` bytes; we write within it while locked.
    unsafe {
        let ptr = GlobalLock(memory).cast::<u8>();
        if ptr.is_null() {
            let _ = GlobalFree(Some(memory));
            bail!("GlobalLock failed");
        }
        let drop_files = DROPFILES {
            pFiles: u32::try_from(header)?,
            fWide: true.into(),
            ..Default::default()
        };
        std::ptr::copy_nonoverlapping(std::ptr::from_ref(&drop_files).cast::<u8>(), ptr, header);
        std::ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), ptr.add(header), wide.len() * 2);
        let _ = GlobalUnlock(memory);
    }

    let owner = owner.map(|h| HWND(h as *mut _));
    // SAFETY: standard clipboard sequence; on success the clipboard owns `memory`, otherwise
    // we free it.
    unsafe {
        if let Err(err) = OpenClipboard(owner) {
            let _ = GlobalFree(Some(memory));
            return Err(err).context("cannot open the clipboard");
        }
        let _ = EmptyClipboard();
        let set = SetClipboardData(u32::from(CF_HDROP.0), Some(HANDLE(memory.0)));
        let _ = CloseClipboard();
        if let Err(err) = set {
            let _ = GlobalFree(Some(memory));
            return Err(err).context("cannot set clipboard data");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    use super::*;

    /// Overwrites the user's clipboard, so it only runs on request.
    #[test]
    #[ignore = "replaces the clipboard contents"]
    fn copy_file_puts_a_readable_file_list_on_the_clipboard() {
        let path = std::env::current_dir().unwrap().join("Cargo.toml");
        let path = path.to_str().unwrap();
        copy_file(None, path).unwrap();
        // SAFETY: standard clipboard read; the handle stays owned by the clipboard.
        let files = unsafe {
            OpenClipboard(None).unwrap();
            let data = GetClipboardData(u32::from(CF_HDROP.0)).unwrap();
            let drop = HDROP(data.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let mut buf = [0u16; 1024];
            let len = DragQueryFileW(drop, 0, Some(&mut buf));
            let _ = CloseClipboard();
            (count, String::from_utf16_lossy(&buf[..len as usize]))
        };
        assert_eq!(files, (1, path.to_owned()));
    }
}
