//! Link creation (SPEC §1.4): symlinks, junctions and hard links.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

use windows::Win32::Foundation::{ERROR_NOT_SAME_DEVICE, ERROR_PRIVILEGE_NOT_HELD, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_NONE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
use windows::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;
use windows::core::{Owned, PCWSTR};

use crate::wide::to_wide_nul;

/// How a picked item is linked into the target folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// `CreateSymbolicLinkW`; needs Developer Mode or elevation. Works across volumes.
    Symlink,
    /// Mount-point reparse point; folders only, no privilege needed, local volumes only.
    Junction,
    /// `CreateHardLinkW`; files only, same volume only.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "selectable via config and the Shift mode popup (next step)"
        )
    )]
    Hardlink,
}

impl fmt::Display for LinkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Symlink => "symlink",
            Self::Junction => "junction",
            Self::Hardlink => "hard link",
        })
    }
}

/// Mode used for files and for folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    pub file: LinkMode,
    pub dir: LinkMode,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            file: LinkMode::Symlink,
            dir: LinkMode::Junction,
        }
    }
}

/// Why a single link could not be created.
#[derive(Debug)]
pub enum LinkError {
    /// Symlinks need Developer Mode or elevation.
    PrivilegeNotHeld,
    /// Hard links must stay on one volume.
    CrossVolume,
    /// A folder cannot be linked into itself or one of its subfolders.
    IntoItself,
    /// The mode does not apply to this item (hard link of a folder, junction of a file).
    WrongKind(LinkMode),
    /// Junction targets must be on a local drive (`C:\...`).
    NotLocal,
    /// The picked item no longer exists.
    SourceMissing,
    Io(io::Error),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrivilegeNotHeld => f.write_str(
                "creating symlinks requires Developer Mode (Settings > System > For developers)",
            ),
            Self::CrossVolume => f.write_str("hard links only work within the same drive"),
            Self::IntoItself => f.write_str("a folder cannot be linked into itself"),
            Self::WrongKind(LinkMode::Hardlink) => f.write_str("hard links only work for files"),
            Self::WrongKind(mode) => write!(f, "a {mode} cannot be used for this item"),
            Self::NotLocal => f.write_str("junctions only work for folders on local drives"),
            Self::SourceMissing => f.write_str("the picked item no longer exists"),
            Self::Io(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for LinkError {}

impl From<io::Error> for LinkError {
    fn from(err: io::Error) -> Self {
        match err.raw_os_error().and_then(|code| u32::try_from(code).ok()) {
            Some(code) if code == ERROR_PRIVILEGE_NOT_HELD.0 => Self::PrivilegeNotHeld,
            Some(code) if code == ERROR_NOT_SAME_DEVICE.0 => Self::CrossVolume,
            _ => Self::Io(err),
        }
    }
}

/// A link that was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub link: PathBuf,
    pub mode: LinkMode,
}

/// Creates a link to `source` inside `target_dir`. On a name collision the link is named
/// `name (link).ext`, then `name (link 2).ext`, and so on.
pub fn link_into(source: &Path, target_dir: &Path, modes: Modes) -> Result<Created, LinkError> {
    let metadata = std::fs::metadata(source).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => LinkError::SourceMissing,
        _ => LinkError::Io(err),
    })?;
    let is_dir = metadata.is_dir();
    let mode = if is_dir { modes.dir } else { modes.file };

    match (mode, is_dir) {
        (LinkMode::Hardlink, true) | (LinkMode::Junction, false) => {
            return Err(LinkError::WrongKind(mode));
        }
        _ => {}
    }
    if is_dir && starts_with_ignore_case(target_dir, source) {
        return Err(LinkError::IntoItself);
    }
    let junction_target = match mode {
        LinkMode::Junction => Some(local_drive_path(source).ok_or(LinkError::NotLocal)?),
        _ => None,
    };

    let name = source
        .file_name()
        .ok_or_else(|| LinkError::Io(io::Error::other("the picked item has no name")))?;
    for attempt in 0..1000 {
        let link = target_dir.join(candidate_name(name, is_dir, attempt));
        let result = match (mode, &junction_target) {
            (LinkMode::Symlink, _) if is_dir => std::os::windows::fs::symlink_dir(source, &link),
            (LinkMode::Symlink, _) => std::os::windows::fs::symlink_file(source, &link),
            (LinkMode::Hardlink, _) => std::fs::hard_link(source, &link),
            (LinkMode::Junction, Some(target)) => create_junction(target, &link),
            (LinkMode::Junction, None) => unreachable_junction(),
        };
        match result {
            Ok(()) => return Ok(Created { link, mode }),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
    }
    Err(LinkError::Io(io::Error::other("too many name collisions")))
}

/// `junction_target` is always `Some` for junctions; kept out of line to avoid a panic path.
#[cold]
fn unreachable_junction() -> io::Result<()> {
    Err(io::Error::other("internal error: missing junction target"))
}

/// `name`, `name (link)`, `name (link 2)`, … with the extension kept for files.
fn candidate_name(name: &std::ffi::OsStr, is_dir: bool, attempt: u32) -> OsString {
    if attempt == 0 {
        return name.to_owned();
    }
    let suffix = if attempt == 1 {
        " (link)".to_owned()
    } else {
        format!(" (link {attempt})")
    };
    let as_path = Path::new(name);
    let (stem, extension) = match (is_dir, as_path.file_stem(), as_path.extension()) {
        (false, Some(stem), Some(ext)) => (stem, Some(ext)),
        _ => (name, None),
    };
    let mut out = stem.to_owned();
    out.push(suffix);
    if let Some(ext) = extension {
        out.push(".");
        out.push(ext);
    }
    out
}

/// Case-insensitive (ASCII + simple Unicode) component-wise `path.starts_with(base)`.
fn starts_with_ignore_case(path: &Path, base: &Path) -> bool {
    let mut path = path.components();
    base.components().all(|b| {
        path.next().is_some_and(|p| {
            p.as_os_str().to_string_lossy().to_lowercase()
                == b.as_os_str().to_string_lossy().to_lowercase()
        })
    })
}

/// Returns `path` as `C:\...` if it is on a drive letter (plain or `\\?\C:\` form).
fn local_drive_path(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    let drive = match components.next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
            _ => return None,
        },
        _ => return None,
    };
    let mut out = PathBuf::from(format!("{}:\\", char::from(drive)));
    out.extend(components.filter(|c| !matches!(c, Component::RootDir)));
    Some(out)
}

/// Creates an empty folder at `link` and turns it into a mount point for `target` (`C:\...`).
fn create_junction(target: &Path, link: &Path) -> io::Result<()> {
    std::fs::create_dir(link)?;
    let result = set_mount_point(link, target);
    if result.is_err() {
        // Best effort: do not leave an empty folder behind.
        let _ = std::fs::remove_dir(link);
    }
    result
}

fn set_mount_point(link: &Path, target: &Path) -> io::Result<()> {
    let buffer = mount_point_buffer(target)?;
    let link_wide = to_wide_nul(link.as_os_str());

    // SAFETY: `link_wide` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(link_wide.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(win_to_io)?;
    // SAFETY: CreateFileW returned a fresh handle that we own; Owned closes it on drop.
    let handle = unsafe { Owned::new(handle) };

    let mut returned = 0u32;
    // SAFETY: `buffer` is a complete REPARSE_DATA_BUFFER of the given length; no output
    // buffer is used and `returned` is a valid out-pointer.
    unsafe {
        DeviceIoControl(
            *handle,
            FSCTL_SET_REPARSE_POINT,
            Some(buffer.as_ptr().cast()),
            u32::try_from(buffer.len()).map_err(io::Error::other)?,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(win_to_io)
}

/// Builds a `REPARSE_DATA_BUFFER` for a mount point:
///
/// ```text
/// u32 ReparseTag, u16 ReparseDataLength, u16 Reserved,
/// u16 SubstituteNameOffset, u16 SubstituteNameLength,
/// u16 PrintNameOffset, u16 PrintNameLength,
/// WCHAR PathBuffer[]   // "\??\C:\target" NUL "C:\target" NUL
/// ```
fn mount_point_buffer(target: &Path) -> io::Result<Vec<u8>> {
    let print: Vec<u16> = to_wide_nul(target.as_os_str());
    let print = &print[..print.len() - 1];
    let substitute: Vec<u16> = r"\??\"
        .encode_utf16()
        .chain(print.iter().copied())
        .collect();

    let too_long = || io::Error::other("junction target path is too long");
    let substitute_bytes = u16::try_from(substitute.len() * 2).map_err(|_| too_long())?;
    let print_bytes = u16::try_from(print.len() * 2).map_err(|_| too_long())?;
    // Path buffer holds both names plus a NUL after each.
    let path_buffer_bytes = substitute_bytes
        .checked_add(print_bytes)
        .and_then(|n| n.checked_add(4))
        .ok_or_else(too_long)?;
    let data_length = path_buffer_bytes.checked_add(8).ok_or_else(too_long)?;

    let mut buf = Vec::with_capacity(8 + usize::from(data_length));
    buf.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buf.extend_from_slice(&data_length.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    buf.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    buf.extend_from_slice(&substitute_bytes.to_le_bytes());
    buf.extend_from_slice(&(substitute_bytes + 2).to_le_bytes()); // PrintNameOffset
    buf.extend_from_slice(&print_bytes.to_le_bytes());
    for unit in substitute.iter().chain(&[0]).chain(print).chain(&[0]) {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(buf)
}

fn win_to_io(err: windows::core::Error) -> io::Error {
    // HRESULT_FROM_WIN32 keeps the Win32 code in the low 16 bits.
    #[allow(clippy::cast_sign_loss)]
    let hresult = err.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        #[allow(clippy::cast_possible_wrap)]
        return io::Error::from_raw_os_error((hresult & 0xFFFF) as i32);
    }
    io::Error::other(err)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A fresh directory under %TEMP%, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ferrofs-link-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn file_in(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn candidate_names_follow_spec() {
        let name = std::ffi::OsStr::new("report.final.pdf");
        assert_eq!(candidate_name(name, false, 0), "report.final.pdf");
        assert_eq!(candidate_name(name, false, 1), "report.final (link).pdf");
        assert_eq!(candidate_name(name, false, 2), "report.final (link 2).pdf");
        assert_eq!(
            candidate_name(std::ffi::OsStr::new("my.dir"), true, 1),
            "my.dir (link)"
        );
        assert_eq!(
            candidate_name(std::ffi::OsStr::new(".gitignore"), false, 1),
            ".gitignore (link)"
        );
    }

    #[test]
    fn local_drive_path_strips_verbatim_prefix_and_rejects_unc() {
        assert_eq!(
            local_drive_path(Path::new(r"\\?\C:\a\b")),
            Some(PathBuf::from(r"C:\a\b"))
        );
        assert_eq!(
            local_drive_path(Path::new(r"D:\x")),
            Some(PathBuf::from(r"D:\x"))
        );
        assert_eq!(local_drive_path(Path::new(r"\\server\share\x")), None);
        assert_eq!(local_drive_path(Path::new(r"relative\x")), None);
    }

    #[test]
    fn mount_point_buffer_layout() {
        let buf = mount_point_buffer(Path::new(r"C:\t")).unwrap();
        // "\??\C:\t" = 8 units, "C:\t" = 4 units, plus two NULs = 14 units = 28 bytes.
        assert_eq!(buf.len(), 8 + 8 + 28);
        assert_eq!(u16::from_le_bytes([buf[4], buf[5]]), 8 + 28); // ReparseDataLength
        assert_eq!(u16::from_le_bytes([buf[10], buf[11]]), 16); // SubstituteNameLength
        assert_eq!(u16::from_le_bytes([buf[12], buf[13]]), 18); // PrintNameOffset
        assert_eq!(u16::from_le_bytes([buf[14], buf[15]]), 8); // PrintNameLength
    }

    #[test]
    fn junction_links_folder_and_resolves() {
        let tmp = TempDir::new("junction");
        let source = tmp.0.join("source");
        fs::create_dir(&source).unwrap();
        file_in(&source, "inner.txt", "hello");
        let target = tmp.0.join("target");
        fs::create_dir(&target).unwrap();

        let created = link_into(&source, &target, Modes::default()).unwrap();
        assert_eq!(created.mode, LinkMode::Junction);
        assert_eq!(created.link, target.join("source"));
        assert_eq!(
            fs::read_to_string(created.link.join("inner.txt")).unwrap(),
            "hello"
        );
        assert!(is_reparse_point(&created.link));

        // Removing the junction must not touch the original folder.
        fs::remove_dir(&created.link).unwrap();
        assert!(source.join("inner.txt").exists());
    }

    #[test]
    fn collisions_get_link_suffix() {
        let tmp = TempDir::new("collide");
        let source = file_in(&tmp.0, "a.txt", "1");
        let modes = Modes {
            file: LinkMode::Hardlink,
            dir: LinkMode::Junction,
        };
        // Linking into the source's own folder collides with the original name.
        let first = link_into(&source, &tmp.0, modes).unwrap();
        let second = link_into(&source, &tmp.0, modes).unwrap();
        assert_eq!(first.link, tmp.0.join("a (link).txt"));
        assert_eq!(second.link, tmp.0.join("a (link 2).txt"));
        assert_eq!(fs::read_to_string(&second.link).unwrap(), "1");
    }

    #[test]
    fn symlink_file_or_privilege_error() {
        let tmp = TempDir::new("symlink");
        let source = file_in(&tmp.0, "s.txt", "sym");
        let target = tmp.0.join("t");
        fs::create_dir(&target).unwrap();
        match link_into(&source, &target, Modes::default()) {
            Ok(created) => {
                assert_eq!(created.mode, LinkMode::Symlink);
                assert!(
                    fs::symlink_metadata(&created.link)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(fs::read_to_string(&created.link).unwrap(), "sym");
            }
            // Without Developer Mode the error must be classified for the user hint.
            Err(LinkError::PrivilegeNotHeld) => {}
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn refuses_folder_into_itself_and_wrong_kinds() {
        let tmp = TempDir::new("refuse");
        let dir = tmp.0.join("d");
        fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(matches!(
            link_into(&dir, &dir.join("sub"), Modes::default()),
            Err(LinkError::IntoItself)
        ));
        let hard_dirs = Modes {
            file: LinkMode::Symlink,
            dir: LinkMode::Hardlink,
        };
        assert!(matches!(
            link_into(&dir, &tmp.0, hard_dirs),
            Err(LinkError::WrongKind(LinkMode::Hardlink))
        ));
        assert!(matches!(
            link_into(&tmp.0.join("missing"), &tmp.0, Modes::default()),
            Err(LinkError::SourceMissing)
        ));
    }

    fn is_reparse_point(path: &Path) -> bool {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        fs::symlink_metadata(path)
            .map(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
            .unwrap_or(false)
    }
}
