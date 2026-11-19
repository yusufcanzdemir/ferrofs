//! Re-reading size, allocation, modification time and attributes of changed files
//! (SPEC §2.4: the USN journal says *that* a file changed, not its new size).

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR,
    FILE_ID_DESCRIPTOR_0, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo, FileIdType, FileStandardInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING, OpenFileById,
};
use windows::core::{Owned, PCWSTR};

use crate::error::{Result, Win32Context};
use crate::index::record_number;

/// `USN_REASON_*` bits after which sizes or times may have changed.
const CONTENT_REASONS: u32 = 0x1 | 0x2 | 0x4 | 0x100 | 0x8000; // overwrite, extend, truncate, create, basic info
const REASON_CLOSE: u32 = 0x8000_0000;
const REASON_DELETE: u32 = 0x200;

/// True for the journal record that closes a burst of content changes: the right moment
/// to read the new size once instead of after every write.
pub fn needs_refresh(reason: u32) -> bool {
    reason & REASON_CLOSE != 0 && reason & CONTENT_REASONS != 0 && reason & REASON_DELETE == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub size: u64,
    pub alloc_size: u64,
    /// FILETIME of the last write.
    pub mtime: u64,
    /// `FILE_ATTRIBUTE_*`.
    pub attributes: u32,
}

/// Opens files by reference number on one volume.
#[derive(Debug)]
pub struct MetadataReader {
    /// Any handle on the volume; `OpenFileById` uses it to pick the volume.
    hint: Owned<HANDLE>,
}

// SAFETY: the hint handle is only passed to OpenFileById, which is thread-safe.
unsafe impl Send for MetadataReader {}

impl MetadataReader {
    /// `root` is the volume root such as `C:\`.
    pub fn open(root: &str) -> Result<Self> {
        let root: Vec<u16> = OsStr::new(root).encode_wide().chain([0]).collect();
        // SAFETY: NUL-terminated path; directories need FILE_FLAG_BACKUP_SEMANTICS.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(root.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .win32("cannot open the volume root")?;
        // SAFETY: a fresh handle we own.
        let hint = unsafe { Owned::new(handle) };
        Ok(Self { hint })
    }

    /// Current metadata of the file, or `None` if it is gone or inaccessible.
    pub fn query(&self, frn: u64) -> Option<Metadata> {
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: u32::try_from(size_of::<FILE_ID_DESCRIPTOR>()).ok()?,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                // The full reference (with sequence number) so a reused record is rejected.
                FileId: i64::from_ne_bytes(frn.to_ne_bytes()),
            },
        };
        // SAFETY: `descriptor` is fully initialised; only attributes are requested.
        let file = unsafe {
            OpenFileById(
                *self.hint,
                &descriptor,
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        };
        let file = match file {
            // SAFETY: a fresh handle we own.
            Ok(handle) => unsafe { Owned::new(handle) },
            Err(err) => {
                tracing::trace!(record = record_number(frn), "cannot open by id: {err}");
                return None;
            }
        };
        let mut basic = FILE_BASIC_INFO::default();
        let mut standard = FILE_STANDARD_INFO::default();
        // SAFETY: each buffer matches the requested information class and size.
        unsafe {
            GetFileInformationByHandleEx(
                *file,
                FileBasicInfo,
                std::ptr::from_mut(&mut basic).cast::<c_void>(),
                u32::try_from(size_of::<FILE_BASIC_INFO>()).ok()?,
            )
            .ok()?;
            GetFileInformationByHandleEx(
                *file,
                FileStandardInfo,
                std::ptr::from_mut(&mut standard).cast::<c_void>(),
                u32::try_from(size_of::<FILE_STANDARD_INFO>()).ok()?,
            )
            .ok()?;
        }
        Some(Metadata {
            size: u64::try_from(standard.EndOfFile).unwrap_or(0),
            alloc_size: u64::try_from(standard.AllocationSize).unwrap_or(0),
            mtime: u64::try_from(basic.LastWriteTime).unwrap_or(0),
            attributes: basic.FileAttributes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_only_on_close_of_content_changes() {
        assert!(needs_refresh(0x8000_0002)); // extend + close
        assert!(needs_refresh(0x8000_0100)); // create + close
        assert!(!needs_refresh(0x0000_0002)); // extend, file still open
        assert!(!needs_refresh(0x8000_0200)); // delete + close
        assert!(!needs_refresh(0x8000_1000)); // rename only
    }
}
