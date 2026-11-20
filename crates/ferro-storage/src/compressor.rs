//! Folder-level WOF compression (SPEC §5.3): scan a folder for files worth compressing,
//! estimate the savings from a sample, compress, and undo (decompress). Runs in this
//! process with the user's own permissions; folders under Program Files need an
//! elevated process.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rayon::prelude::*;

use crate::actions::{ActionLog, Field};
use crate::wof::{self, Algorithm, Backing, Estimator};

/// Files smaller than this cannot save a whole cluster.
const MIN_SIZE: u64 = 8 << 10;
/// Bytes compressed in memory for an estimate.
const SAMPLE_BUDGET: u64 = 128 << 20;
/// Most bytes read from one file for an estimate.
const SAMPLE_PER_FILE: usize = 8 << 20;
/// Worker threads (compression is CPU-bound in the kernel, reading is IO-bound).
const THREADS: usize = 4;
/// Most failures kept with their message.
const MAX_FAILURES: usize = 50;

/// Formats that are compressed already; WOF would only cost CPU on them.
const COMPRESSED_FORMATS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "avif", "mp4", "mkv", "avi", "mov", "webm", "wmv",
    "mp3", "aac", "flac", "ogg", "opus", "m4a", "wma", "zip", "7z", "rar", "gz", "tgz", "bz2",
    "xz", "zst", "lz4", "cab", "msi", "msp", "pdf", "docx", "xlsx", "pptx", "jar", "apk", "appx",
    "msix", "iso", "vhd", "vhdx", "wim", "esd", "bik", "bk2", "usm", "woff", "woff2", "nupkg",
    "whl", "crate",
];

/// Shared with the UI thread to draw progress and to cancel.
#[derive(Debug, Default)]
pub struct Progress {
    pub files_done: AtomicU64,
    pub files_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    fn reset(&self, files: u64, bytes: u64) {
        self.files_done.store(0, Ordering::Relaxed);
        self.bytes_done.store(0, Ordering::Relaxed);
        self.files_total.store(files, Ordering::Relaxed);
        self.bytes_total.store(bytes, Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn step(&self, bytes: u64) {
        self.files_done.fetch_add(1, Ordering::Relaxed);
        self.bytes_done.fetch_add(bytes, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub size: u64,
}

/// Files and bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Count {
    pub files: u64,
    pub bytes: u64,
}

impl Count {
    fn add(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes += bytes;
    }
}

/// What a folder contains, as far as compression is concerned.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub folder: String,
    pub candidates: Vec<Candidate>,
    /// Logical size of all files.
    pub total: Count,
    /// Bytes on disk of all files now.
    pub on_disk: u64,
    /// Compressed already (WOF or NTFS compression).
    pub already: Count,
    pub formats: Count,
    pub small: Count,
    pub unreadable: Count,
    /// Newest modification among the candidates (FILETIME), for "rarely updated".
    pub newest: u64,
    pub cancelled: bool,
}

impl Scan {
    pub fn candidate_bytes(&self) -> u64 {
        self.candidates.iter().map(|c| c.size).sum()
    }
}

/// Lower-case extension of a path.
fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// FILETIME of a modification time.
fn filetime(time: std::io::Result<std::time::SystemTime>) -> u64 {
    const UNIX_TO_FILETIME: u64 = 116_444_736_000_000_000;
    time.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| UNIX_TO_FILETIME + d.as_secs() * 10_000_000)
}

/// Walks `folder` (links are not followed) and sorts its files into candidates and the
/// reasons others are skipped.
pub fn scan(folder: &str, progress: &Progress) -> Scan {
    let mut result = Scan {
        folder: folder.to_owned(),
        ..Scan::default()
    };
    progress.reset(0, 0);
    let mut pending = vec![PathBuf::from(folder)];
    while let Some(dir) = pending.pop() {
        if progress.cancelled() {
            result.cancelled = true;
            return result;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            result.unreadable.files += 1;
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                result.unreadable.files += 1;
                continue;
            };
            let path = entry.path();
            if meta.is_dir() {
                if !meta.file_type().is_symlink() && !is_reparse_point(&meta) {
                    pending.push(path);
                }
                continue;
            }
            if meta.file_type().is_symlink() {
                continue;
            }
            let size = meta.len();
            let text = path.to_string_lossy().into_owned();
            let disk = wof::size_on_disk(&text).unwrap_or(size);
            result.total.add(size);
            result.on_disk += disk;
            progress.step(size);
            if size < MIN_SIZE {
                result.small.add(size);
            } else if disk < size.saturating_sub(4096) {
                result.already.add(size);
            } else if COMPRESSED_FORMATS.contains(&extension(&path).as_str()) {
                result.formats.add(size);
            } else {
                result.newest = result.newest.max(filetime(meta.modified()));
                result.candidates.push(Candidate { path: text, size });
            }
        }
    }
    result
}

fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

/// Expected result of compressing the candidates, from a sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    pub algorithm: Algorithm,
    pub sampled: u64,
    /// Stored bytes of the sample after compression.
    pub sampled_stored: u64,
    /// Candidate bytes before and (estimated) after.
    pub before: u64,
    pub after: u64,
    pub cancelled: bool,
}

impl Estimate {
    pub fn saved(&self) -> u64 {
        self.before.saturating_sub(self.after)
    }
}

/// Compresses a deterministic sample of the candidates in memory (at most 128 MB, 8 MB per
/// file, spread over the list) and extrapolates.
pub fn estimate(candidates: &[Candidate], algorithm: Algorithm, progress: &Progress) -> Estimate {
    let before: u64 = candidates.iter().map(|c| c.size).sum();
    // Every k-th file so the sample spans the folder, not just its first subfolder.
    let total_read: u64 = candidates
        .iter()
        .map(|c| c.size.min(SAMPLE_PER_FILE as u64))
        .sum();
    let step = usize::try_from(total_read / SAMPLE_BUDGET + 1)
        .unwrap_or(1)
        .max(1);
    let sample: Vec<&Candidate> = candidates.iter().step_by(step).collect();
    let sample_bytes: u64 = sample
        .iter()
        .map(|c| c.size.min(SAMPLE_PER_FILE as u64))
        .sum();
    progress.reset(sample.len() as u64, sample_bytes);

    let run = || {
        sample
            .par_iter()
            .map_init(
                || Estimator::new(algorithm).ok(),
                |estimator, candidate| {
                    if progress.cancelled() {
                        return (0, 0);
                    }
                    let Some(estimator) = estimator.as_mut() else {
                        return (0, 0);
                    };
                    let mut data = Vec::new();
                    let read = File::open(&candidate.path).and_then(|file| {
                        use std::io::Read;
                        file.take(SAMPLE_PER_FILE as u64).read_to_end(&mut data)
                    });
                    progress.step(data.len() as u64);
                    match read {
                        Ok(n) if n > 0 => (n as u64, estimator.stored_size(&data)),
                        _ => (0, 0),
                    }
                },
            )
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    };
    let (sampled, sampled_stored) = match pool() {
        Some(pool) => pool.install(run),
        None => run(),
    };
    // Whole clusters on disk: a stored file never shrinks below its rounding.
    let after = if sampled == 0 {
        before
    } else {
        let ratio = sampled_stored as f64 / sampled as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let after = (before as f64 * ratio.min(1.0)) as u64;
        after
    };
    Estimate {
        algorithm,
        sampled,
        sampled_stored,
        before,
        after,
        cancelled: progress.cancelled(),
    }
}

use std::fs::File;

fn pool() -> Option<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(THREADS)
        .thread_name(|i| format!("compress-{i}"))
        .build()
        .ok()
}

/// Outcome of compressing or decompressing a folder.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Files changed.
    pub done: Count,
    /// Bytes on disk of those files before and after.
    pub disk_before: u64,
    pub disk_after: u64,
    /// Files left alone (already in the requested state, other WOF provider).
    pub unchanged: u64,
    pub failed: u64,
    /// First failures with their reason.
    pub failures: Vec<(String, String)>,
    /// Some failures were "access denied" (an elevated process may succeed).
    pub access_denied: bool,
    pub cancelled: bool,
    pub dry_run: bool,
}

/// Refuses folders WOF must not be applied to file by file.
pub fn check_folder(folder: &str, windir: Option<&str>) -> Result<(), String> {
    let path = Path::new(folder);
    let deep = path
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count()
        >= 2;
    if !deep {
        return Err(
            "pick a folder at least two levels deep (not a drive or top-level folder)".into(),
        );
    }
    if let Some(windir) = windir {
        let windir = windir.trim_end_matches('\\').to_lowercase();
        let lower = folder.trim_end_matches('\\').to_lowercase();
        if lower == windir || lower.starts_with(&format!("{windir}\\")) {
            return Err(
                "Windows system files are compressed with `compact /compactos`, not per file"
                    .into(),
            );
        }
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
            "no access to {folder}; administrator rights may help"
        )),
        _ => Err(format!("{folder} is not a folder")),
    }
}

/// Compresses the candidates. With `dry_run` nothing is changed; the run is only logged.
pub fn compress(
    folder: &str,
    candidates: &[Candidate],
    algorithm: Algorithm,
    dry_run: bool,
    log: &ActionLog,
    progress: &Progress,
) -> Summary {
    let summary = if dry_run {
        Summary {
            dry_run: true,
            ..Summary::default()
        }
    } else {
        run_over(candidates, progress, |path| match wof::backing(path)? {
            Backing::None => wof::compress(path, algorithm).map(|()| true),
            Backing::Compressed(_) | Backing::Other => Ok(false),
        })
    };
    record(
        log,
        "compress",
        folder,
        Some(algorithm),
        candidates.len(),
        &summary,
    );
    summary
}

/// Decompresses every WOF-compressed file below `folder` (the undo of [`compress`]).
pub fn decompress(folder: &str, dry_run: bool, log: &ActionLog, progress: &Progress) -> Summary {
    // Every file, whatever its size or type: earlier runs may have used other rules.
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::from(folder)];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() && !meta.file_type().is_symlink() && !is_reparse_point(&meta) {
                pending.push(entry.path());
            } else if meta.is_file() {
                files.push(Candidate {
                    path: entry.path().to_string_lossy().into_owned(),
                    size: meta.len(),
                });
            }
        }
    }
    let summary = if dry_run {
        Summary {
            dry_run: true,
            ..Summary::default()
        }
    } else {
        run_over(&files, progress, |path| match wof::backing(path)? {
            Backing::Compressed(_) => wof::decompress(path).map(|()| true),
            Backing::None | Backing::Other => Ok(false),
        })
    };
    record(log, "decompress", folder, None, files.len(), &summary);
    summary
}

/// Applies `change` to each file in parallel; `change` returns whether it did anything.
fn run_over(
    files: &[Candidate],
    progress: &Progress,
    change: impl Fn(&str) -> std::io::Result<bool> + Sync,
) -> Summary {
    progress.reset(files.len() as u64, files.iter().map(|f| f.size).sum());
    let summary = Mutex::new(Summary::default());
    let run = || {
        files.par_iter().for_each(|file| {
            if progress.cancelled() {
                return;
            }
            let before = wof::size_on_disk(&file.path).unwrap_or(file.size);
            let result = change(&file.path);
            let after = wof::size_on_disk(&file.path).unwrap_or(before);
            progress.step(file.size);
            let mut s = summary
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match result {
                Ok(true) => {
                    s.done.add(file.size);
                    s.disk_before += before;
                    s.disk_after += after;
                }
                Ok(false) => s.unchanged += 1,
                Err(err) => {
                    s.failed += 1;
                    s.access_denied |= err.kind() == std::io::ErrorKind::PermissionDenied
                        || err.raw_os_error() == Some(5);
                    if s.failures.len() < MAX_FAILURES {
                        s.failures.push((file.path.clone(), err.to_string()));
                    }
                }
            }
        });
    };
    match pool() {
        Some(pool) => pool.install(run),
        None => run(),
    }
    let mut summary = summary
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    summary.cancelled = progress.cancelled();
    summary
}

/// One log line per folder run: undo is "decompress the folder".
fn record(
    log: &ActionLog,
    action: &str,
    folder: &str,
    algorithm: Option<Algorithm>,
    files: usize,
    summary: &Summary,
) {
    let result = if summary.dry_run {
        "dry-run".to_owned()
    } else if summary.cancelled {
        "cancelled".to_owned()
    } else if summary.failed > 0 {
        format!("partial: {} failed", summary.failed)
    } else {
        "ok".to_owned()
    };
    log.append(&[
        ("tool", Field::Str("compression")),
        ("action", Field::Str(action)),
        ("path", Field::Str(folder)),
        (
            "algorithm",
            Field::Str(algorithm.map_or("", Algorithm::name)),
        ),
        ("files", Field::Num(files as u64)),
        ("changed", Field::Num(summary.done.files)),
        ("disk_before", Field::Num(summary.disk_before)),
        ("disk_after", Field::Num(summary.disk_after)),
        ("failed", Field::Num(summary.failed)),
        ("dry_run", Field::Bool(summary.dry_run)),
        ("result", Field::Str(&result)),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasher::tests::TempDir;

    fn text(len: usize) -> Vec<u8> {
        b"<assembly name=\"Contoso.Widgets\" version=\"1.0.0.0\" />\n"
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect()
    }

    /// Compresses, estimates and restores files in a %TEMP% fixture.
    #[test]
    fn scan_estimate_compress_and_undo() {
        let dir = TempDir::new("compressor");
        let app = dir.0.join("App");
        std::fs::create_dir_all(app.join("bin")).unwrap();
        std::fs::write(app.join("bin").join("core.dll"), text(3 << 20)).unwrap();
        std::fs::write(app.join("data.xml"), text(1 << 20)).unwrap();
        std::fs::write(app.join("intro.mp4"), text(1 << 20)).unwrap(); // skipped by type
        std::fs::write(app.join("tiny.ini"), b"[x]").unwrap();
        let folder = app.to_string_lossy().into_owned();
        let progress = Progress::default();

        let found = scan(&folder, &progress);
        assert_eq!(found.total.files, 4);
        assert_eq!(found.candidates.len(), 2);
        assert_eq!(found.formats.files, 1);
        assert_eq!(found.small.files, 1);
        assert!(found.newest > 0);

        let guess = estimate(&found.candidates, Algorithm::Xpress8k, &progress);
        assert_eq!(guess.before, 4 << 20);
        assert!(guess.after < guess.before / 4, "{guess:?}");

        let log = ActionLog::new(dir.0.join("actions.jsonl"));
        let dry = compress(
            &folder,
            &found.candidates,
            Algorithm::Xpress8k,
            true,
            &log,
            &progress,
        );
        assert!(dry.dry_run && dry.done.files == 0);
        let done = compress(
            &folder,
            &found.candidates,
            Algorithm::Xpress8k,
            false,
            &log,
            &progress,
        );
        assert_eq!(
            (done.done.files, done.failed),
            (2, 0),
            "{:?}",
            done.failures
        );
        assert!(done.disk_after < done.disk_before / 4);
        assert_eq!(std::fs::read(app.join("data.xml")).unwrap(), text(1 << 20));

        // A second scan sees them as compressed already.
        let again = scan(&folder, &progress);
        assert_eq!(again.already.files, 2);
        assert!(again.candidates.is_empty());

        let undone = decompress(&folder, false, &log, &progress);
        assert_eq!(undone.done.files, 2);
        assert_eq!(undone.unchanged, 2);
        let restored = scan(&folder, &progress);
        assert_eq!(restored.candidates.len(), 2);

        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"dry_run\":true"));
        assert!(
            lines[1].contains("\"action\":\"compress\"")
                && lines[1].contains("\"algorithm\":\"XPRESS8K\"")
        );
        assert!(
            lines[2].contains("\"action\":\"decompress\"")
                && lines[2].contains("\"result\":\"ok\"")
        );
    }

    #[test]
    fn folder_checks() {
        let windir = Some(r"C:\Windows");
        assert!(check_folder(r"C:\", windir).is_err());
        assert!(check_folder(r"C:\Games", windir).is_err());
        assert!(
            check_folder(r"C:\Windows\System32", windir)
                .unwrap_err()
                .contains("compactos")
        );
        assert!(
            check_folder(r"Q:\nope\missing", windir)
                .unwrap_err()
                .contains("not a folder")
        );
        let dir = TempDir::new("compressor-check");
        assert!(check_folder(&dir.0.to_string_lossy(), windir).is_ok());
    }
}
