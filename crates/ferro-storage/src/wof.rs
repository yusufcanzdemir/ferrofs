//! Windows Overlay Filter (WOF) file compression, the mechanism behind `compact /exe`
//! (SPEC §5.3). A compressed file stays readable by every program; it is decompressed
//! transparently on read, and stored uncompressed again when it is written. Undo is
//! `FSCTL_DELETE_EXTERNAL_BACKING`.
//!
//! Savings are estimated before compressing by compressing samples in memory with the
//! Windows Compression API in WOF's chunk size: XPRESS with Huffman for the XPRESS modes
//! (the same family WOF uses) and LZMS as a stand-in for LZX (an approximation).

use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

use windows::Win32::Foundation::{ERROR_OBJECT_NOT_EXTERNALLY_BACKED, HANDLE};
use windows::Win32::Storage::Compression::{
    COMPRESS_ALGORITHM, COMPRESS_ALGORITHM_LZMS, COMPRESS_ALGORITHM_XPRESS_HUFF, COMPRESSOR_HANDLE,
    CloseCompressor, Compress, CreateCompressor,
};
use windows::Win32::Storage::FileSystem::{
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetCompressedFileSizeW, INVALID_FILE_SIZE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FILE_PROVIDER_CURRENT_VERSION, FILE_PROVIDER_EXTERNAL_INFO_V1, FSCTL_DELETE_EXTERNAL_BACKING,
    FSCTL_GET_EXTERNAL_BACKING, FSCTL_SET_EXTERNAL_BACKING, WOF_CURRENT_VERSION, WOF_EXTERNAL_INFO,
};
use windows::core::PCWSTR;

/// `WOF_PROVIDER_FILE`: per-file compression (the other providers are WIM and cloud).
const WOF_PROVIDER_FILE: u32 = 2;

/// WOF compression algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algorithm {
    Xpress4k,
    Xpress8k,
    Xpress16k,
    Lzx,
}

impl Algorithm {
    pub const ALL: [Self; 4] = [Self::Xpress4k, Self::Xpress8k, Self::Xpress16k, Self::Lzx];

    /// `FILE_PROVIDER_COMPRESSION_*`.
    fn code(self) -> u32 {
        match self {
            Self::Xpress4k => 0,
            Self::Lzx => 1,
            Self::Xpress8k => 2,
            Self::Xpress16k => 3,
        }
    }

    /// Bytes compressed independently (a read decompresses whole chunks).
    pub fn chunk(self) -> usize {
        match self {
            Self::Xpress4k => 4 << 10,
            Self::Xpress8k => 8 << 10,
            Self::Xpress16k => 16 << 10,
            Self::Lzx => 32 << 10,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Xpress4k => "XPRESS4K",
            Self::Xpress8k => "XPRESS8K",
            Self::Xpress16k => "XPRESS16K",
            Self::Lzx => "LZX",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Xpress4k => "XPRESS4K: fastest, least savings",
            Self::Xpress8k => "XPRESS8K: balanced (compact /exe default)",
            Self::Xpress16k => "XPRESS16K: more savings, a bit slower",
            Self::Lzx => "LZX: most savings, slow to compress; best for rarely updated apps",
        }
    }

    fn estimator(self) -> COMPRESS_ALGORITHM {
        match self {
            Self::Lzx => COMPRESS_ALGORITHM_LZMS,
            _ => COMPRESS_ALGORITHM_XPRESS_HUFF,
        }
    }
}

/// WOF state of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// Ordinary file.
    None,
    /// Compressed by WOF with this algorithm code.
    Compressed(u32),
    /// Backed by another provider (WIM image, cloud): left alone.
    Other,
}

fn wide(path: &str) -> Vec<u16> {
    OsStr::new(path).encode_wide().chain([0]).collect()
}

/// Opens a file for WOF control codes: read access only, everything shared. A handle with
/// write access is not just unnecessary: opening a WOF file for writing makes WOF drop its
/// compression (observed), which is also why updated program files end up uncompressed.
fn open_read(path: &str) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .open(path)
}

fn ioctl(file: &File, code: u32, input: &[u8], output: &mut [u8]) -> windows::core::Result<u32> {
    let mut returned = 0u32;
    // SAFETY: the handle belongs to `file`; the buffers are live slices of the sizes given;
    // the call is synchronous (no OVERLAPPED).
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            code,
            (!input.is_empty()).then_some(input.as_ptr().cast()),
            u32::try_from(input.len()).unwrap_or(0),
            (!output.is_empty()).then_some(output.as_mut_ptr().cast()),
            u32::try_from(output.len()).unwrap_or(0),
            Some(&mut returned),
            None,
        )
    }?;
    Ok(returned)
}

/// Bytes of a plain-old-data struct.
fn bytes_of<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: `T` is a `#[repr(C)]` struct of `u32` fields (no padding, no pointers);
    // the slice covers exactly its size and lives as long as the borrow.
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(value).cast::<u8>(), size_of::<T>()) }
}

pub fn backing(path: &str) -> std::io::Result<Backing> {
    let file = open_read(path)?;
    let mut out =
        [0u8; size_of::<WOF_EXTERNAL_INFO>() + size_of::<FILE_PROVIDER_EXTERNAL_INFO_V1>()];
    match ioctl(&file, FSCTL_GET_EXTERNAL_BACKING, &[], &mut out) {
        Ok(_) => {
            let provider = u32::from_le_bytes([out[4], out[5], out[6], out[7]]);
            if provider == WOF_PROVIDER_FILE {
                Ok(Backing::Compressed(u32::from_le_bytes([
                    out[12], out[13], out[14], out[15],
                ])))
            } else {
                Ok(Backing::Other)
            }
        }
        Err(err) if err.code() == ERROR_OBJECT_NOT_EXTERNALLY_BACKED.to_hresult() => {
            Ok(Backing::None)
        }
        Err(err) => Err(std::io::Error::other(err)),
    }
}

/// Compresses one file in place (synchronously; large files take a while).
pub fn compress(path: &str, algorithm: Algorithm) -> std::io::Result<()> {
    let file = open_read(path)?;
    let wof = WOF_EXTERNAL_INFO {
        Version: WOF_CURRENT_VERSION,
        Provider: WOF_PROVIDER_FILE,
    };
    let provider = FILE_PROVIDER_EXTERNAL_INFO_V1 {
        Version: FILE_PROVIDER_CURRENT_VERSION,
        Algorithm: algorithm.code(),
        Flags: 0,
    };
    let mut input = Vec::with_capacity(20);
    input.extend_from_slice(bytes_of(&wof));
    input.extend_from_slice(bytes_of(&provider));
    ioctl(&file, FSCTL_SET_EXTERNAL_BACKING, &input, &mut [])
        .map(|_| ())
        .map_err(std::io::Error::other)
}

/// Restores a WOF-compressed file to an ordinary one.
pub fn decompress(path: &str) -> std::io::Result<()> {
    let file = open_read(path)?;
    ioctl(&file, FSCTL_DELETE_EXTERNAL_BACKING, &[], &mut [])
        .map(|_| ())
        .map_err(std::io::Error::other)
}

/// Bytes the file occupies on disk (compressed size for WOF and NTFS compression).
pub fn size_on_disk(path: &str) -> std::io::Result<u64> {
    let name = wide(path);
    let mut high = 0u32;
    // SAFETY: `name` is NUL-terminated and outlives the call; `high` is a live local.
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(name.as_ptr()), Some(&mut high)) };
    if low == INVALID_FILE_SIZE {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error().unwrap_or(0) != 0 {
            return Err(err);
        }
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

/// In-memory compressor used for estimates; closed on drop.
pub struct Estimator {
    handle: COMPRESSOR_HANDLE,
    chunk: usize,
    out: Vec<u8>,
}

impl Estimator {
    pub fn new(algorithm: Algorithm) -> std::io::Result<Self> {
        let mut handle = COMPRESSOR_HANDLE::default();
        // SAFETY: `handle` is a live local; on success we own it and close it on drop.
        unsafe { CreateCompressor(algorithm.estimator(), None, &mut handle) }
            .map_err(std::io::Error::other)?;
        Ok(Self {
            handle,
            chunk: algorithm.chunk(),
            out: vec![0u8; algorithm.chunk()],
        })
    }

    /// Estimated stored size of `data` (one file's bytes): each chunk compressed on its
    /// own, kept raw when compression does not help, plus an 8-byte chunk table entry.
    pub fn stored_size(&mut self, data: &[u8]) -> u64 {
        let mut total = 0u64;
        for chunk in data.chunks(self.chunk) {
            let mut size = 0usize;
            // SAFETY: input and output are live buffers of the sizes passed.
            let ok = unsafe {
                Compress(
                    self.handle,
                    Some(chunk.as_ptr().cast()),
                    chunk.len(),
                    Some(self.out.as_mut_ptr().cast()),
                    self.out.len(),
                    &mut size,
                )
            }
            .is_ok();
            let stored = if ok && size < chunk.len() {
                size
            } else {
                chunk.len()
            };
            total += stored as u64 + 8;
        }
        total
    }
}

impl Drop for Estimator {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateCompressor and is closed exactly once.
        let _ = unsafe { CloseCompressor(self.handle) };
    }
}

// The handle is only used through `&mut self`.
// SAFETY: compressor handles are not tied to the creating thread.
unsafe impl Send for Estimator {}

impl std::fmt::Debug for Estimator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Estimator")
            .field("chunk", &self.chunk)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasher::tests::TempDir;

    fn text(len: usize) -> Vec<u8> {
        b"fn main() { println!(\"hello, compressed world\"); }\n"
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect()
    }

    fn random(len: usize) -> Vec<u8> {
        let mut x = 0x1234_5678_9abc_def0u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn estimates_separate_text_from_noise() {
        for algorithm in Algorithm::ALL {
            let mut estimator = Estimator::new(algorithm).unwrap();
            let len = 1 << 20;
            let packed = estimator.stored_size(&text(len));
            let noise = estimator.stored_size(&random(len));
            assert!(packed < len as u64 / 4, "{algorithm:?}: {packed}");
            assert!(noise >= len as u64, "{algorithm:?}: {noise}");
        }
    }

    /// Compresses and restores a file in %TEMP% (NTFS on C:).
    #[test]
    fn compress_and_decompress_a_temporary_file() {
        let dir = TempDir::new("wof");
        let data = text(2 << 20);
        let path = dir.file("app.dll", &data);
        assert_eq!(backing(&path).unwrap(), Backing::None);
        let before = size_on_disk(&path).unwrap();
        assert_eq!(before, data.len() as u64);

        compress(&path, Algorithm::Xpress8k).unwrap();
        assert_eq!(backing(&path).unwrap(), Backing::Compressed(2));
        let after = size_on_disk(&path).unwrap();
        assert!(after < before / 4, "{after} vs {before}");
        assert_eq!(std::fs::read(&path).unwrap(), data, "reads are transparent");

        decompress(&path).unwrap();
        assert_eq!(backing(&path).unwrap(), Backing::None);
        assert_eq!(size_on_disk(&path).unwrap(), before);
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }
}
