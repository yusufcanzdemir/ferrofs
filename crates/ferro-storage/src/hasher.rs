//! Confirms duplicate candidates by content (SPEC §5.2 steps 2–4), in this process and
//! with the user's own permissions:
//!
//! 1. Open each candidate, check that its size still matches, and read its file ID (two
//!    names of one file, i.e. hard links, are the same file and never duplicates).
//! 2. Partial hash: xxh3-128 of the first and last 64 KB. Files up to 128 KB are read
//!    whole here and get their full hash right away.
//! 3. Full hash: streaming BLAKE3 for files whose partial hash is shared.
//!
//! Work runs on a small dedicated thread pool (disks do not like dozens of readers) and
//! can be cancelled between reads.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use rayon::prelude::*;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetFileInformationByHandle,
};

/// Bytes read from each end of a file for the partial hash.
pub const EDGE: usize = 64 * 1024;
/// Reader threads.
const THREADS: usize = 4;
/// Read size for full hashes.
const BUFFER: usize = 1 << 20;

/// A file of a size group, as reported by the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub size: u64,
}

/// Identity and state of a file when it was hashed; actions re-check it before touching
/// the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileState {
    pub volume_serial: u32,
    pub file_id: u64,
    pub size: u64,
    /// Last write time (FILETIME).
    pub mtime: u64,
}

impl FileState {
    /// Current state of the file at `path`.
    pub fn read(path: &str) -> std::io::Result<Self> {
        let file = open(path)?;
        state_of(&file)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupFile {
    pub path: String,
    pub state: FileState,
}

/// Files with identical content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupGroup {
    pub size: u64,
    pub blake3: [u8; 32],
    pub files: Vec<DupFile>,
}

impl DupGroup {
    /// Bytes freed if all but one copy went away.
    pub fn wasted(&self) -> u64 {
        self.size
            .saturating_mul(self.files.len().saturating_sub(1) as u64)
    }

    pub fn hash_hex(&self) -> String {
        self.blake3.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Most wasted bytes first.
    pub groups: Vec<DupGroup>,
    /// Files that could not be compared, with the reason.
    pub skipped: Vec<(String, String)>,
    pub cancelled: bool,
    /// Names that turned out to be hard links of a file already in the comparison.
    pub already_linked: usize,
}

/// Shared with the UI thread, which polls it to draw progress.
#[derive(Debug, Default)]
pub struct Progress {
    /// 0 = partial hashes, 1 = full hashes, 2 = finished.
    pub phase: AtomicU8,
    pub files_done: AtomicU64,
    pub files_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn start_phase(&self, phase: u8, files: u64, bytes: u64) {
        self.files_done.store(0, Ordering::Relaxed);
        self.bytes_done.store(0, Ordering::Relaxed);
        self.files_total.store(files, Ordering::Relaxed);
        self.bytes_total.store(bytes, Ordering::Relaxed);
        self.phase.store(phase, Ordering::Relaxed);
    }
}

/// Result of the first pass over one file.
struct Probe {
    state: FileState,
    partial: u128,
    full: Option<[u8; 32]>,
}

/// Groups `groups` (candidates of equal size) into sets of files with identical content.
pub fn confirm(groups: Vec<Vec<Candidate>>, progress: &Progress) -> Report {
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(THREADS)
        .thread_name(|i| format!("hasher-{i}"))
        .build()
    {
        Ok(pool) => pool,
        Err(err) => {
            return Report {
                skipped: vec![(String::new(), format!("cannot start reader threads: {err}"))],
                ..Report::default()
            };
        }
    };
    pool.install(|| confirm_on_pool(groups, progress))
}

fn confirm_on_pool(groups: Vec<Vec<Candidate>>, progress: &Progress) -> Report {
    let mut report = Report::default();
    let files: Vec<(usize, Candidate)> = groups
        .into_iter()
        .enumerate()
        .flat_map(|(g, files)| files.into_iter().map(move |f| (g, f)))
        .collect();
    let edge_bytes: u64 = files.iter().map(|(_, f)| f.size.min(2 * EDGE as u64)).sum();
    progress.start_phase(0, files.len() as u64, edge_bytes);

    // Pass 1: identity, size check and partial hash.
    let probes: Vec<(usize, Candidate, Result<Probe, String>)> = files
        .into_par_iter()
        .map(|(g, file)| {
            let result = if progress.cancelled() {
                Err("cancelled".to_owned())
            } else {
                probe(&file, progress).map_err(|e| e.to_string())
            };
            progress.files_done.fetch_add(1, Ordering::Relaxed);
            (g, file, result)
        })
        .collect();
    if progress.cancelled() {
        report.cancelled = true;
        return report;
    }

    // Same size group + same partial hash, one entry per file ID.
    let mut by_partial: HashMap<(usize, u128), Vec<(Candidate, Probe)>> = HashMap::new();
    for (g, file, result) in probes {
        match result {
            Ok(probe) => {
                let bucket = by_partial.entry((g, probe.partial)).or_default();
                let same_file = bucket.iter().any(|(_, p)| {
                    p.state.volume_serial == probe.state.volume_serial
                        && p.state.file_id == probe.state.file_id
                });
                if same_file {
                    report.already_linked += 1;
                } else {
                    bucket.push((file, probe));
                }
            }
            Err(reason) => report.skipped.push((file.path, reason)),
        }
    }
    let mut pending: Vec<(Candidate, Probe)> = by_partial
        .into_values()
        .filter(|bucket| bucket.len() >= 2)
        .flatten()
        .collect();

    // Pass 2: full hashes where the partial read did not cover the whole file.
    let full_bytes: u64 = pending
        .iter()
        .filter(|(_, p)| p.full.is_none())
        .map(|(f, _)| f.size)
        .sum();
    let full_files = pending.iter().filter(|(_, p)| p.full.is_none()).count() as u64;
    progress.start_phase(1, full_files, full_bytes);
    let failures: Vec<(usize, String)> = pending
        .par_iter_mut()
        .enumerate()
        .filter(|(_, (_, probe))| probe.full.is_none())
        .filter_map(|(i, (file, probe))| {
            let result = full_hash(&file.path, probe.state, progress);
            progress.files_done.fetch_add(1, Ordering::Relaxed);
            match result {
                Ok(hash) => {
                    probe.full = Some(hash);
                    None
                }
                Err(err) => Some((i, err.to_string())),
            }
        })
        .collect();
    if progress.cancelled() {
        report.cancelled = true;
        return report;
    }
    for (i, reason) in &failures {
        report
            .skipped
            .push((pending[*i].0.path.clone(), reason.clone()));
    }

    let mut by_full: HashMap<(u64, [u8; 32]), Vec<DupFile>> = HashMap::new();
    for (file, probe) in pending {
        if let Some(hash) = probe.full {
            by_full.entry((file.size, hash)).or_default().push(DupFile {
                path: file.path,
                state: probe.state,
            });
        }
    }
    report.groups = by_full
        .into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|((size, blake3), mut files)| {
            files.sort_by(|a, b| a.path.cmp(&b.path));
            DupGroup {
                size,
                blake3,
                files,
            }
        })
        .collect();
    report
        .groups
        .sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.blake3.cmp(&b.blake3)));
    progress.phase.store(2, Ordering::Relaxed);
    report
}

fn open(path: &str) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN.0)
        .open(path)
}

fn state_of(file: &File) -> std::io::Result<FileState> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle belongs to `file`, which outlives the call; `info` is a live local.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(std::io::Error::other)?;
    Ok(FileState {
        volume_serial: info.dwVolumeSerialNumber,
        file_id: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        mtime: (u64::from(info.ftLastWriteTime.dwHighDateTime) << 32)
            | u64::from(info.ftLastWriteTime.dwLowDateTime),
    })
}

fn changed() -> std::io::Error {
    std::io::Error::other("changed since it was indexed")
}

fn probe(candidate: &Candidate, progress: &Progress) -> std::io::Result<Probe> {
    let mut file = open(&candidate.path)?;
    let state = state_of(&file)?;
    if state.size != candidate.size {
        return Err(changed());
    }
    let size = usize::try_from(state.size).unwrap_or(usize::MAX);
    if size <= 2 * EDGE {
        let mut data = Vec::with_capacity(size);
        file.read_to_end(&mut data)?;
        if data.len() != size {
            return Err(changed());
        }
        progress
            .bytes_done
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        return Ok(Probe {
            state,
            partial: xxhash_rust::xxh3::xxh3_128(&data),
            full: Some(*blake3::hash(&data).as_bytes()),
        });
    }
    let mut edges = vec![0u8; 2 * EDGE];
    file.read_exact(&mut edges[..EDGE])?;
    file.seek(SeekFrom::End(-(EDGE as i64)))?;
    file.read_exact(&mut edges[EDGE..])?;
    progress
        .bytes_done
        .fetch_add(edges.len() as u64, Ordering::Relaxed);
    Ok(Probe {
        state,
        partial: xxhash_rust::xxh3::xxh3_128(&edges),
        full: None,
    })
}

fn full_hash(path: &str, expected: FileState, progress: &Progress) -> std::io::Result<[u8; 32]> {
    let mut file = open(path)?;
    let state = state_of(&file)?;
    if state != expected {
        return Err(changed());
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; BUFFER];
    let mut total = 0u64;
    loop {
        if progress.cancelled() {
            return Err(std::io::Error::other("cancelled"));
        }
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        total += n as u64;
        progress.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
    }
    if total != expected.size {
        return Err(changed());
    }
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// A fresh folder under %TEMP% (never a real user folder), removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ferrofs-test-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub fn file(&self, name: &str, data: &[u8]) -> String {
            let path = self.0.join(name);
            std::fs::write(&path, data).unwrap();
            path.to_str().unwrap().to_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn candidate(path: &str) -> Candidate {
        Candidate {
            path: path.to_owned(),
            size: std::fs::metadata(path).unwrap().len(),
        }
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn confirms_identical_files_and_rejects_lookalikes() {
        let dir = TempDir::new("hasher");
        let big = pattern(300_000, 1);
        let a = dir.file("a.bin", &big);
        let b = dir.file("b.bin", &big);
        // Same size, same first and last 64 KB, different middle: partial hashes collide.
        let mut middle = big.clone();
        middle[150_000] ^= 0xFF;
        let c = dir.file("c.bin", &middle);
        let small = pattern(1000, 7);
        let s1 = dir.file("s1.txt", &small);
        let s2 = dir.file("s2.txt", &small);
        let s3 = dir.file("s3.txt", &pattern(1000, 8));
        // A hard link to a.bin is the same file, not a duplicate.
        let link = dir.0.join("a-link.bin");
        std::fs::hard_link(&a, &link).unwrap();
        let link = link.to_str().unwrap().to_owned();
        let gone = dir.0.join("missing.bin").to_str().unwrap().to_owned();

        let progress = Progress::default();
        let report = confirm(
            vec![
                vec![
                    candidate(&a),
                    candidate(&b),
                    candidate(&c),
                    candidate(&link),
                ],
                vec![candidate(&s1), candidate(&s2), candidate(&s3)],
                vec![
                    Candidate {
                        path: gone.clone(),
                        size: 5,
                    },
                    Candidate {
                        path: s1.clone(),
                        size: 999, // stale size from the index
                    },
                ],
            ],
            &progress,
        );
        assert!(!report.cancelled);
        assert_eq!(report.groups.len(), 2, "{:?}", report.groups);
        let big_group = &report.groups[0];
        assert_eq!(big_group.size, 300_000);
        assert_eq!(big_group.files.len(), 2);
        assert!(big_group.files.iter().any(|f| f.path == b));
        assert!(!big_group.files.iter().any(|f| f.path == c));
        assert_eq!(big_group.wasted(), 300_000);
        assert_eq!(big_group.hash_hex().len(), 64);
        assert_eq!(report.groups[1].files.len(), 2);
        let skipped: Vec<_> = report.skipped.iter().map(|(p, _)| p.as_str()).collect();
        assert!(skipped.contains(&gone.as_str()) && skipped.contains(&s1.as_str()));
        assert_eq!(progress.phase.load(Ordering::Relaxed), 2);
        assert_eq!(report.already_linked, 1);
        assert!(Path::new(&a).exists(), "hashing never modifies files");
    }

    #[test]
    fn cancel_stops_early() {
        let dir = TempDir::new("cancel");
        let data = pattern(200_000, 3);
        let a = dir.file("a.bin", &data);
        let b = dir.file("b.bin", &data);
        let progress = Progress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        let report = confirm(vec![vec![candidate(&a), candidate(&b)]], &progress);
        assert!(report.cancelled);
        assert!(report.groups.is_empty());
    }
}
