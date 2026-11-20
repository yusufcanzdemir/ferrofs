//! Destructive actions on duplicates and the action log (CLAUDE.md safety rules):
//!
//! - Before touching a file, both it and the copy that is kept are re-checked against the
//!   state recorded when they were hashed (same file ID, size and write time), so a file
//!   that changed meanwhile is never removed and the last copy is never lost.
//! - Removed files go to the Recycle Bin through `IFileOperation` unless permanent
//!   deletion was chosen explicitly. If a file cannot be recycled (e.g. no Recycle Bin on
//!   that volume) the shell asks before deleting it for good (`FOF_WANTNUKEWARNING`).
//! - Hard link replacement creates the link under a temporary name first, removes the
//!   duplicate, then renames the link into place, so a failure never leaves the path
//!   empty without a copy elsewhere.
//! - Every action, including dry runs, is appended to `%LOCALAPPDATA%\ferrofs\actions.jsonl`.

use std::ffi::OsStr;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::{CreateHardLinkW, MOVE_FILE_FLAGS, MoveFileExW};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FOF_WANTNUKEWARNING, FOFX_RECYCLEONDELETE,
    FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
};
use windows::core::PCWSTR;

use crate::hasher::FileState;

/// What happens to a duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Remove it; the kept copy stays.
    Remove,
    /// Replace it with a hard link to the kept copy (same volume only).
    HardLink,
}

/// How removed files are disposed of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposal {
    RecycleBin,
    Permanent,
}

/// One planned action on one duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub kind: Kind,
    pub path: String,
    pub state: FileState,
    /// The identical copy that is kept.
    pub keep: String,
    pub keep_state: FileState,
    /// BLAKE3 of the content (hex), recorded in the log.
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Dry run: only logged.
    Logged,
    /// Not attempted because a check failed (nothing was changed).
    Skipped(String),
    Failed(String),
}

/// Runs `plan` in order. Must be called on a thread with an STA COM apartment
/// (`IFileOperation`). `owner` parents shell dialogs.
pub fn execute(
    plan: &[Planned],
    disposal: Disposal,
    dry_run: bool,
    log: &ActionLog,
    owner: Option<HWND>,
    mut on_done: impl FnMut(usize, &Outcome),
) -> Vec<Outcome> {
    let mut outcomes = Vec::with_capacity(plan.len());
    for (i, item) in plan.iter().enumerate() {
        let outcome = match verify(item) {
            Err(reason) => Outcome::Skipped(reason),
            Ok(()) if dry_run => Outcome::Logged,
            Ok(()) => match run(item, disposal, owner) {
                Ok(()) => Outcome::Done,
                Err(err) => Outcome::Failed(format!("{err:#}")),
            },
        };
        log.record(item, disposal, dry_run, &outcome);
        on_done(i, &outcome);
        outcomes.push(outcome);
    }
    outcomes
}

/// Both files must be unchanged since they were hashed, and distinct.
fn verify(item: &Planned) -> Result<(), String> {
    if item.state.file_id == item.keep_state.file_id
        && item.state.volume_serial == item.keep_state.volume_serial
    {
        return Err("already the same file as the kept copy".into());
    }
    match FileState::read(&item.keep) {
        Ok(state) if state == item.keep_state => {}
        Ok(_) => return Err(format!("the kept copy {} has changed", item.keep)),
        Err(err) => {
            return Err(format!(
                "the kept copy {} is not readable: {err}",
                item.keep
            ));
        }
    }
    match FileState::read(&item.path) {
        Ok(state) if state == item.state => {}
        Ok(_) => return Err("changed since it was compared".into()),
        Err(err) => return Err(format!("not readable: {err}")),
    }
    if item.kind == Kind::HardLink && item.state.volume_serial != item.keep_state.volume_serial {
        return Err("hard links need both files on the same volume".into());
    }
    Ok(())
}

fn run(item: &Planned, disposal: Disposal, owner: Option<HWND>) -> Result<()> {
    match item.kind {
        Kind::Remove => dispose(&item.path, disposal, owner),
        Kind::HardLink => replace_with_link(&item.path, &item.keep, disposal, owner),
    }
}

fn dispose(path: &str, disposal: Disposal, owner: Option<HWND>) -> Result<()> {
    match disposal {
        Disposal::RecycleBin => recycle(path, owner),
        Disposal::Permanent => {
            std::fs::remove_file(path).with_context(|| format!("cannot delete {path}"))
        }
    }
}

fn wide(path: &str) -> Vec<u16> {
    OsStr::new(path).encode_wide().chain([0]).collect()
}

/// Moves one file to the Recycle Bin.
pub fn recycle(path: &str, owner: Option<HWND>) -> Result<()> {
    let file = wide(path);
    // SAFETY: COM is initialized on this thread (see `execute`); `file` is NUL-terminated
    // and outlives the calls; the interfaces are released when they drop.
    unsafe {
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_ALL).context("IFileOperation")?;
        operation.SetOperationFlags(
            FOF_ALLOWUNDO
                | FOF_NOCONFIRMATION
                | FOF_SILENT
                | FOF_WANTNUKEWARNING
                | windows::Win32::UI::Shell::FILEOPERATION_FLAGS(FOFX_RECYCLEONDELETE.0),
        )?;
        if let Some(owner) = owner {
            operation.SetOwnerWindow(owner)?;
        }
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(file.as_ptr()), None)
            .with_context(|| format!("cannot resolve {path}"))?;
        operation.DeleteItem(&item, None)?;
        operation.PerformOperations().context("the shell refused")?;
        if operation.GetAnyOperationsAborted()?.as_bool() {
            bail!("cancelled");
        }
    }
    if Path::new(path).exists() {
        bail!("still present after moving it to the Recycle Bin");
    }
    Ok(())
}

/// Temporary name next to `path` for the new link.
fn temp_link_path(path: &str) -> PathBuf {
    let path = Path::new(path);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.ferrofs-{}.tmp", std::process::id()))
}

fn replace_with_link(
    path: &str,
    keep: &str,
    disposal: Disposal,
    owner: Option<HWND>,
) -> Result<()> {
    let temp = temp_link_path(path);
    let temp_text = temp.to_string_lossy().into_owned();
    let (temp_w, keep_w, path_w) = (wide(&temp_text), wide(keep), wide(path));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe { CreateHardLinkW(PCWSTR(temp_w.as_ptr()), PCWSTR(keep_w.as_ptr()), None) }
        .with_context(|| format!("cannot create a hard link to {keep}"))?;
    if let Err(err) = dispose(path, disposal, owner) {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MoveFileExW(
            PCWSTR(temp_w.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            MOVE_FILE_FLAGS(0),
        )
    }
    .with_context(|| format!("the link was left at {temp_text}"))?;
    Ok(())
}

/// Append-only JSON Lines log of destructive actions.
#[derive(Debug, Clone)]
pub struct ActionLog {
    path: PathBuf,
}

impl ActionLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `%LOCALAPPDATA%\ferrofs\actions.jsonl`.
    pub fn default_file() -> Option<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(base).join("ferrofs").join("actions.jsonl"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn record(&self, item: &Planned, disposal: Disposal, dry_run: bool, outcome: &Outcome) {
        let line = entry(item, disposal, dry_run, outcome, SystemTime::now());
        let written = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?;
                file.write_all(line.as_bytes())
            });
        if let Err(err) = written {
            tracing::error!("cannot write the action log {}: {err}", self.path.display());
        }
    }
}

/// One log line (with the trailing newline). Enough to undo: recycled files are restored
/// from the Recycle Bin by `path`; a hard-linked path gets its own copy of `kept` back,
/// whose content is identical (same `blake3`).
fn entry(
    item: &Planned,
    disposal: Disposal,
    dry_run: bool,
    outcome: &Outcome,
    now: SystemTime,
) -> String {
    let action = match (item.kind, disposal) {
        (Kind::Remove, Disposal::RecycleBin) => "recycle",
        (Kind::Remove, Disposal::Permanent) => "delete",
        (Kind::HardLink, Disposal::RecycleBin) => "hardlink-recycle-original",
        (Kind::HardLink, Disposal::Permanent) => "hardlink-delete-original",
    };
    let result = match outcome {
        Outcome::Done => "ok".to_owned(),
        Outcome::Logged => "dry-run".to_owned(),
        Outcome::Skipped(reason) => format!("skipped: {reason}"),
        Outcome::Failed(reason) => format!("failed: {reason}"),
    };
    format!(
        "{{\"time\":{},\"tool\":\"duplicates\",\"action\":{},\"path\":{},\"kept\":{},\"size\":{},\"blake3\":{},\"dry_run\":{},\"result\":{}}}\n",
        json_string(&utc_timestamp(now)),
        json_string(action),
        json_string(&item.path),
        json_string(&item.keep),
        item.state.size,
        json_string(&item.hash),
        dry_run,
        json_string(&result),
    )
}

/// JSON string literal (RFC 8259 escaping).
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `2026-11-20T10:00:00Z`.
fn utc_timestamp(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::hasher::tests::TempDir;

    fn plan(kind: Kind, path: &str, keep: &str) -> Planned {
        Planned {
            kind,
            path: path.to_owned(),
            state: FileState::read(path).unwrap(),
            keep: keep.to_owned(),
            keep_state: FileState::read(keep).unwrap(),
            hash: "ab".into(),
        }
    }

    #[test]
    fn permanent_remove_and_hard_link_replacement() {
        let dir = TempDir::new("actions");
        let keep = dir.file("keep.bin", b"same content");
        let dup = dir.file("dup.bin", b"same content");
        let linked = dir.file("linked.bin", b"same content");
        let log = ActionLog::new(dir.0.join("log").join("actions.jsonl"));

        let items = [
            plan(Kind::Remove, &dup, &keep),
            plan(Kind::HardLink, &linked, &keep),
        ];
        // Dry run: nothing changes, both are logged.
        let outcomes = execute(&items, Disposal::Permanent, true, &log, None, |_, _| {});
        assert_eq!(outcomes, [Outcome::Logged, Outcome::Logged]);
        assert!(Path::new(&dup).exists());

        let outcomes = execute(&items, Disposal::Permanent, false, &log, None, |_, _| {});
        assert_eq!(outcomes, [Outcome::Done, Outcome::Done]);
        assert!(!Path::new(&dup).exists());
        assert_eq!(std::fs::read(&linked).unwrap(), b"same content");
        let (a, b) = (
            FileState::read(&linked).unwrap(),
            FileState::read(&keep).unwrap(),
        );
        assert_eq!(a.file_id, b.file_id, "now one file with two names");
        let leftovers: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());

        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(
            lines[0].contains("\"dry_run\":true") && lines[0].contains("\"result\":\"dry-run\"")
        );
        assert!(
            lines[2].contains("\"action\":\"delete\"") && lines[2].contains("\"result\":\"ok\"")
        );
        assert!(lines[3].contains("\"action\":\"hardlink-delete-original\""));

        // A second run finds the link already in place and skips it.
        let again = execute(
            &items[1..],
            Disposal::Permanent,
            false,
            &log,
            None,
            |_, _| {},
        );
        assert!(
            matches!(&again[0], Outcome::Skipped(r) if r.contains("changed") || r.contains("same file"))
        );
    }

    #[test]
    fn changed_files_are_skipped() {
        let dir = TempDir::new("actions-changed");
        let keep = dir.file("keep.bin", b"content");
        let dup = dir.file("dup.bin", b"content");
        let log = ActionLog::new(dir.0.join("actions.jsonl"));
        let item = plan(Kind::Remove, &dup, &keep);

        // The kept copy changes: the duplicate must not be removed.
        std::fs::write(&keep, b"edited!").unwrap();
        let outcomes = execute(
            std::slice::from_ref(&item),
            Disposal::Permanent,
            false,
            &log,
            None,
            |_, _| {},
        );
        assert!(matches!(&outcomes[0], Outcome::Skipped(r) if r.contains("kept copy")));
        assert!(Path::new(&dup).exists());

        // The kept copy is gone.
        std::fs::remove_file(&keep).unwrap();
        let outcomes = execute(&[item], Disposal::Permanent, false, &log, None, |_, _| {});
        assert!(matches!(&outcomes[0], Outcome::Skipped(_)));
        assert!(Path::new(&dup).exists());
    }

    /// Puts a tiny temporary file into the user's Recycle Bin, so it only runs on request:
    /// `cargo test -p ferro-storage -- --ignored recycle`.
    #[test]
    #[ignore = "moves a temporary file to the user's Recycle Bin"]
    fn recycle_moves_a_temporary_file_to_the_recycle_bin() {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        // SAFETY: first COM call on this test thread (IFileOperation needs an STA).
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let dir = TempDir::new("recycle");
        let path = dir.file(
            "ferrofs-recycle-test.txt",
            b"ferrofs test file, safe to delete",
        );
        recycle(&path, None).unwrap();
        assert!(!Path::new(&path).exists());
    }

    #[test]
    fn log_formatting() {
        assert_eq!(
            json_string("C:\\a \"b\"\n\u{1}"),
            "\"C:\\\\a \\\"b\\\"\\n\\u0001\""
        );
        assert_eq!(utc_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_795_000_000);
        assert_eq!(utc_timestamp(t), "2026-11-18T11:06:40Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert!(utc_timestamp(leap).starts_with("2000-02-29"));
    }
}
