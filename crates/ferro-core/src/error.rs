//! Error type of ferro-core.

/// Errors from volume access and the USN journal.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Win32 {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    /// The journal was deleted, recreated or wrapped past our position.
    #[error("the USN journal was reset; a full rescan is needed")]
    JournalReset,
    #[error("{0} is not an NTFS volume")]
    NotNtfs(String),
    #[error("{0} is not a volume root such as `C:\\`")]
    BadRoot(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Adds a static description to a `windows` error.
pub(crate) trait Win32Context<T> {
    fn win32(self, context: &'static str) -> Result<T>;
}

impl<T> Win32Context<T> for windows::core::Result<T> {
    fn win32(self, context: &'static str) -> Result<T> {
        self.map_err(|source| Error::Win32 { context, source })
    }
}

impl Error {
    /// Win32 error code, if this wraps a Win32 failure.
    pub fn win32_code(&self) -> Option<u32> {
        match self {
            Self::Win32 { source, .. } => win32_code(source),
            _ => None,
        }
    }
}

/// Win32 error code inside a `windows` error (HRESULT_FROM_WIN32 keeps it in the low
/// 16 bits).
pub(crate) fn win32_code(err: &windows::core::Error) -> Option<u32> {
    #[allow(clippy::cast_sign_loss)]
    let hresult = err.code().0 as u32;
    (hresult & 0xFFFF_0000 == 0x8007_0000).then_some(hresult & 0xFFFF)
}
