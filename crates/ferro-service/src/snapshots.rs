//! The snapshot folder `%PROGRAMDATA%\ferrofs\index`. Snapshots list every file name on
//! the volume, so the folder is created readable only by SYSTEM and administrators
//! (`%PROGRAMDATA%` itself is readable by all users).

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::CreateDirectoryW;
use windows::core::{Owned, PCWSTR};

/// Protected DACL: full control for SYSTEM and Administrators only, inherited by files.
const PRIVATE_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

fn wide_nul(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}

/// Creates (if needed) and returns the snapshot folder.
pub fn prepare_dir() -> Result<PathBuf> {
    let base = std::env::var_os("PROGRAMDATA").context("PROGRAMDATA is not set")?;
    let parent = PathBuf::from(base).join("ferrofs");
    std::fs::create_dir_all(&parent)?;
    let dir = parent.join("index");

    let sddl = wide_nul(OsStr::new(PRIVATE_SDDL));
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: NUL-terminated SDDL; on success we own the LocalAlloc'd descriptor.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .context("invalid snapshot folder SDDL")?;
    // SAFETY: allocated by the call above with LocalAlloc; freed by Owned.
    let _descriptor_owner = unsafe { Owned::new(HLOCAL(descriptor.0)) };
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())?,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let path = wide_nul(dir.as_os_str());
    // SAFETY: NUL-terminated path and a valid security descriptor for the whole call.
    match unsafe { CreateDirectoryW(PCWSTR(path.as_ptr()), Some(&attributes)) } {
        Ok(()) => tracing::info!(dir = %dir.display(), "created private snapshot folder"),
        Err(err) if err.code() == ERROR_ALREADY_EXISTS.to_hresult() => {}
        Err(err) => return Err(err).context("cannot create the snapshot folder"),
    }
    Ok(dir)
}
