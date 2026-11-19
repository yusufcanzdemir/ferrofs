//! Ensures only one ferro-link runs per user session.

use anyhow::Result;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::{Owned, w};

/// Held for the lifetime of the process; the mutex is released on drop.
#[derive(Debug)]
pub struct SingleInstance(#[allow(dead_code)] Owned<HANDLE>);

/// Returns `None` if another instance already holds the mutex.
pub fn acquire() -> Result<Option<SingleInstance>> {
    // SAFETY: the name is a static NUL-terminated literal and no security attributes are passed.
    let handle = unsafe { CreateMutexW(None, false, w!("Local\\ferrofs-link")) }?;
    // SAFETY: no preconditions; must run right after CreateMutexW to see its status.
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    // SAFETY: CreateMutexW returned a fresh handle that we own exclusively.
    let handle = unsafe { Owned::new(handle) };
    Ok((!already_running).then_some(SingleInstance(handle)))
}
