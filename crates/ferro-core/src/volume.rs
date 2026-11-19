//! Volume discovery and raw volume handles (SPEC §2.1).

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};

use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, OPEN_EXISTING,
};
use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
use windows::core::{Owned, PCWSTR};

use crate::error::{Error, Result, Win32Context};

/// A mounted volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Drive root such as `C:\`.
    pub root: String,
    /// Stable identity such as `\\?\Volume{…}\` (drive letters can change).
    pub guid_path: String,
    pub filesystem: String,
    pub label: String,
    pub serial: u32,
}

impl VolumeInfo {
    pub fn is_ntfs(&self) -> bool {
        self.filesystem.eq_ignore_ascii_case("NTFS")
    }
}

/// Fixed (non-removable, non-network) volumes with a drive letter.
pub fn fixed_volumes() -> Result<Vec<VolumeInfo>> {
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is a valid writable buffer; the API never writes past its length.
    let len = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
    if len == 0 || len > buf.len() {
        return Err(windows::core::Error::from_thread()).win32("GetLogicalDriveStringsW failed");
    }
    let mut volumes = Vec::new();
    for root in buf[..len].split(|&c| c == 0).filter(|s| !s.is_empty()) {
        let root_nul: Vec<u16> = root.iter().copied().chain([0]).collect();
        // SAFETY: `root_nul` is NUL-terminated and outlives the call.
        if unsafe { GetDriveTypeW(PCWSTR(root_nul.as_ptr())) } != DRIVE_FIXED {
            continue;
        }
        let root = String::from_utf16_lossy(root);
        match volume_info(&root) {
            Ok(info) => volumes.push(info),
            Err(err) => tracing::warn!(root, "skipping volume: {err}"),
        }
    }
    Ok(volumes)
}

/// Details of the volume mounted at `root` (e.g. `C:\`).
pub fn volume_info(root: &str) -> Result<VolumeInfo> {
    let root_nul = wide_nul(root);
    let mut label = [0u16; 261];
    let mut filesystem = [0u16; 261];
    let mut serial = 0u32;
    // SAFETY: `root_nul` is NUL-terminated; all output buffers are valid for their lengths.
    unsafe {
        GetVolumeInformationW(
            PCWSTR(root_nul.as_ptr()),
            Some(&mut label),
            Some(&mut serial),
            None,
            None,
            Some(&mut filesystem),
        )
    }
    .win32("GetVolumeInformationW failed")?;

    let mut guid = [0u16; 64];
    // SAFETY: `root_nul` is NUL-terminated and ends with a backslash; `guid` is writable.
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root_nul.as_ptr()), &mut guid) }
        .win32("GetVolumeNameForVolumeMountPointW failed")?;

    Ok(VolumeInfo {
        root: root.to_owned(),
        guid_path: from_wide_nul(&guid),
        filesystem: from_wide_nul(&filesystem),
        label: from_wide_nul(&label),
        serial,
    })
}

/// An open volume device (`\\.\C:`), required for MFT/USN access. Needs elevation.
#[derive(Debug)]
pub struct Volume {
    handle: Owned<HANDLE>,
    root: String,
    overlapped: bool,
}

impl Volume {
    /// Opens the volume whose root is `root` (`C:\`, or a GUID path `\\?\Volume{…}\`).
    pub fn open(root: &str) -> Result<Self> {
        Self::open_with(root, false)
    }

    /// Like [`Volume::open`], but for overlapped I/O so reads can time out and be cancelled.
    pub fn open_overlapped(root: &str) -> Result<Self> {
        Self::open_with(root, true)
    }

    fn open_with(root: &str, overlapped: bool) -> Result<Self> {
        let flags = if overlapped {
            FILE_FLAG_OVERLAPPED
        } else {
            FILE_FLAGS_AND_ATTRIBUTES(0)
        };
        let device = device_path(root).ok_or_else(|| Error::BadRoot(root.to_owned()))?;
        let device_nul = wide_nul(&device);
        // SAFETY: `device_nul` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(device_nul.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                flags,
                None,
            )
        }
        .win32("cannot open the volume (administrator rights are required)")?;
        Ok(Self {
            // SAFETY: CreateFileW returned a fresh handle that we own; Owned closes it.
            handle: unsafe { Owned::new(handle) },
            root: root.to_owned(),
            overlapped,
        })
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub(crate) fn handle(&self) -> HANDLE {
        *self.handle
    }

    pub(crate) fn is_overlapped(&self) -> bool {
        self.overlapped
    }
}

/// `C:\` → `\\.\C:`; `\\?\Volume{…}\` → `\\?\Volume{…}`.
fn device_path(root: &str) -> Option<String> {
    let bytes = root.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some(format!(
            r"\\.\{}:",
            char::from(bytes[0].to_ascii_uppercase())
        ));
    }
    if root.starts_with(r"\\?\Volume{") {
        return Some(root.trim_end_matches('\\').to_owned());
    }
    None
}

fn wide_nul(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain([0]).collect()
}

fn from_wide_nul(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..len])
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_paths() {
        assert_eq!(device_path(r"C:\").as_deref(), Some(r"\\.\C:"));
        assert_eq!(device_path("d:").as_deref(), Some(r"\\.\D:"));
        assert_eq!(
            device_path(r"\\?\Volume{1234}\").as_deref(),
            Some(r"\\?\Volume{1234}")
        );
        assert_eq!(device_path(r"\\server\share\"), None);
    }

    #[test]
    fn lists_the_system_volume() {
        let volumes = fixed_volumes().unwrap();
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let c = volumes
            .iter()
            .find(|v| v.root.starts_with(&system))
            .expect("system volume is listed");
        assert!(c.guid_path.starts_with(r"\\?\Volume{"));
        assert!(!c.filesystem.is_empty());
    }
}
