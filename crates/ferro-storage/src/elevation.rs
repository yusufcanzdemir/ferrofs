//! Administrator rights: some cleanup locations (under `C:\Windows`) can only be emptied by
//! an elevated process, so the tool can restart itself elevated.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{Owned, PCWSTR, w};

/// True if this process runs with an elevated (administrator) token.
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo handle of the current process needs no closing; on success we own
    // `token` and close it through `Owned`.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    // SAFETY: a fresh handle we own.
    let token = unsafe { Owned::new(token) };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: the buffer is a live TOKEN_ELEVATION of the size passed.
    let ok = unsafe {
        GetTokenInformation(
            *token,
            TokenElevation,
            Some(std::ptr::from_mut(&mut elevation).cast()),
            u32::try_from(size_of::<TOKEN_ELEVATION>()).unwrap_or(0),
            &mut returned,
        )
    };
    ok.is_ok() && elevation.TokenIsElevated != 0
}

/// Starts this program again elevated with `args` (UAC prompt). Returns an error if the
/// user declined or the start failed; the caller exits on success.
pub fn relaunch_elevated(args: &str) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find this program")?;
    let exe: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let args: Vec<u16> = OsStr::new(args).encode_wide().chain([0]).collect();
    // SAFETY: all strings are NUL-terminated and outlive the call.
    let code = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(exe.as_ptr()),
            PCWSTR(args.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    // Values <= 32 are errors (e.g. the UAC prompt was cancelled).
    if code.0 as isize <= 32 {
        bail!("not restarted (error {})", code.0 as isize);
    }
    Ok(())
}
