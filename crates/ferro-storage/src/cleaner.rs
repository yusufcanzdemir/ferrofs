//! Runs a confirmed cleanup (SPEC §5.4) under the safety rules of `actions`: each target is
//! re-checked right before it is touched, removed items go to the Recycle Bin unless
//! permanent deletion was chosen, and every target is logged to `actions.jsonl`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HWND;

use crate::actions::{self, ActionLog, Disposal, Field};
use crate::locations::Scope;

/// One location or artifact to clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub label: String,
    pub path: String,
    pub scope: Scope,
    /// File that must still exist (project marker of a developer artifact); empty = none.
    pub marker: String,
    /// Size when scanned, for the log.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Done; `left` items inside could not be removed (in use, access denied).
    Done {
        left: usize,
    },
    /// Dry run: only logged.
    Logged,
    /// Not attempted because a check failed (nothing was changed).
    Skipped(String),
    Failed(String),
}

/// Runs `targets` in order. Must be called on a thread with an STA COM apartment
/// (`IFileOperation`). `owner` parents shell dialogs.
pub fn execute(
    targets: &[Target],
    disposal: Disposal,
    dry_run: bool,
    log: &ActionLog,
    owner: Option<HWND>,
    mut on_done: impl FnMut(usize, &Outcome),
) -> Vec<Outcome> {
    let mut outcomes = Vec::with_capacity(targets.len());
    for (i, target) in targets.iter().enumerate() {
        let outcome = match verify(target) {
            Err(reason) => Outcome::Skipped(reason),
            Ok(()) if dry_run => Outcome::Logged,
            Ok(()) => match clean(target, disposal, owner) {
                Ok(left) => Outcome::Done { left },
                Err(err) => Outcome::Failed(format!("{err:#}")),
            },
        };
        let result = match &outcome {
            Outcome::Done { left: 0 } => "ok".to_owned(),
            Outcome::Done { left } => format!("partial: {left} items left"),
            Outcome::Logged => "dry-run".to_owned(),
            Outcome::Skipped(reason) => format!("skipped: {reason}"),
            Outcome::Failed(reason) => format!("failed: {reason}"),
        };
        log.append(&[
            ("tool", Field::Str("cleanup")),
            (
                "action",
                Field::Str(match disposal {
                    Disposal::RecycleBin => "recycle",
                    Disposal::Permanent => "delete",
                }),
            ),
            ("scope", Field::Str(target.scope.describe())),
            ("label", Field::Str(&target.label)),
            ("path", Field::Str(&target.path)),
            ("size", Field::Num(target.size)),
            ("dry_run", Field::Bool(dry_run)),
            ("result", Field::Str(&result)),
        ]);
        on_done(i, &outcome);
        outcomes.push(outcome);
    }
    outcomes
}

/// The target must still look like what was scanned.
fn verify(target: &Target) -> Result<(), String> {
    let path = Path::new(&target.path);
    // Never a drive root or a top-level folder such as C:\Windows: at least two folder
    // names deep. (`components()` also yields the `C:` prefix and the root separator,
    // so only normal components are counted.)
    if !is_deep_enough(path) {
        return Err("refusing to clean a top-level folder".into());
    }
    let meta = std::fs::symlink_metadata(path).map_err(|err| format!("not found: {err}"))?;
    if meta.file_type().is_symlink() || is_reparse_point(&meta) {
        return Err("it is a link now".into());
    }
    match target.scope {
        Scope::File if !meta.is_file() => return Err("not a file any more".into()),
        Scope::Contents | Scope::Folder if !meta.is_dir() => {
            return Err("not a folder any more".into());
        }
        _ => {}
    }
    if !target.marker.is_empty() && !Path::new(&target.marker).exists() {
        return Err(format!("its project file {} is gone", target.marker));
    }
    Ok(())
}

/// True if `path` has at least two folder or file names after the drive (`C:\a\b`).
fn is_deep_enough(path: &Path) -> bool {
    path.components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count()
        >= 2
}

fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

/// Removes the target; returns how many items inside were left behind.
fn clean(target: &Target, disposal: Disposal, owner: Option<HWND>) -> Result<usize> {
    let path = target.path.as_str();
    match (target.scope, disposal) {
        (Scope::Contents, _) => {
            let children: Vec<String> = std::fs::read_dir(path)
                .with_context(|| format!("cannot list {path}"))?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path().to_string_lossy().into_owned())
                .collect();
            match disposal {
                Disposal::RecycleBin => {
                    // The shell may stop early on an item in use; count what is left.
                    let result = actions::recycle_many(&children, owner);
                    let left = children.iter().filter(|c| Path::new(c).exists()).count();
                    if left == children.len() && !children.is_empty() {
                        result?;
                    }
                    Ok(left)
                }
                Disposal::Permanent => Ok(children.iter().filter(|c| remove(c).is_err()).count()),
            }
        }
        (Scope::Folder | Scope::File, Disposal::RecycleBin) => {
            actions::recycle(path, owner)?;
            Ok(0)
        }
        (Scope::Folder | Scope::File, Disposal::Permanent) => {
            remove(path)?;
            if Path::new(path).exists() {
                bail!("parts of it could not be removed");
            }
            Ok(0)
        }
    }
}

/// Deletes a file or a folder tree for good (links are removed, not followed).
fn remove(path: &str) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() && !meta.file_type().is_symlink() && !is_reparse_point(&meta) {
        std::fs::remove_dir_all(path)
    } else if meta.is_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::fs::OpenOptionsExt;

    use super::*;
    use crate::hasher::tests::TempDir;

    fn target(path: &Path, scope: Scope, marker: &str) -> Target {
        Target {
            label: "test".into(),
            path: path.to_string_lossy().into_owned(),
            scope,
            marker: marker.into(),
            size: 1,
        }
    }

    #[test]
    fn contents_folders_and_files_are_removed_for_good() {
        let dir = TempDir::new("cleaner");
        let tmp = dir.0.join("tmp");
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("a.txt"), b"x").unwrap();
        std::fs::write(tmp.join("sub").join("b.txt"), b"x").unwrap();
        // A file open without sharing cannot be deleted: it is left behind.
        std::fs::write(tmp.join("busy.txt"), b"x").unwrap();
        let busy = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(tmp.join("busy.txt"))
            .unwrap();

        let project = dir.0.join("app");
        std::fs::create_dir_all(project.join("target").join("debug")).unwrap();
        std::fs::write(project.join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(project.join("target").join("debug").join("app.exe"), b"x").unwrap();
        let marker = project.join("Cargo.toml").to_string_lossy().into_owned();
        std::fs::write(dir.0.join("dump.dmp"), b"x").unwrap();

        let log = ActionLog::new(dir.0.join("log").join("actions.jsonl"));
        let targets = [
            target(&tmp, Scope::Contents, ""),
            target(&project.join("target"), Scope::Folder, &marker),
            target(&dir.0.join("dump.dmp"), Scope::File, ""),
        ];
        let dry = execute(&targets, Disposal::Permanent, true, &log, None, |_, _| {});
        assert!(dry.iter().all(|o| *o == Outcome::Logged));
        assert!(tmp.join("a.txt").exists());

        let outcomes = execute(&targets, Disposal::Permanent, false, &log, None, |_, _| {});
        assert_eq!(
            outcomes,
            [
                Outcome::Done { left: 1 },
                Outcome::Done { left: 0 },
                Outcome::Done { left: 0 }
            ]
        );
        assert!(tmp.exists(), "a contents clean keeps the folder");
        assert!(!tmp.join("a.txt").exists() && !tmp.join("sub").exists());
        assert!(tmp.join("busy.txt").exists());
        assert!(!project.join("target").exists());
        assert!(project.join("Cargo.toml").exists());
        assert!(!dir.0.join("dump.dmp").exists());
        drop(busy);

        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 6);
        assert!(
            lines[3].contains("\"tool\":\"cleanup\"") && lines[3].contains("partial: 1 items left")
        );
        assert!(
            lines[4].contains("\"scope\":\"whole folder\"")
                && lines[4].contains("\"result\":\"ok\"")
        );
    }

    /// Pure path check: no file system access.
    #[test]
    fn top_level_paths_are_never_deep_enough() {
        for path in [
            r"C:\",
            r"C:\Windows",
            r"C:\Users",
            r"\\?\C:\Windows",
            r"D:\data\",
        ] {
            assert!(!is_deep_enough(Path::new(path)), "{path}");
        }
        for path in [
            r"C:\Windows\Temp",
            r"C:\Users\me\AppData\Local\Temp",
            r"D:\dev\app\target",
        ] {
            assert!(is_deep_enough(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn checks_refuse_changed_targets() {
        let dir = TempDir::new("cleaner-checks");
        let project = dir.0.join("app");
        std::fs::create_dir_all(project.join("target")).unwrap();
        let log = ActionLog::new(dir.0.join("actions.jsonl"));
        let missing_marker = project.join("Cargo.toml").to_string_lossy().into_owned();
        let targets = [
            target(&project.join("target"), Scope::Folder, &missing_marker),
            target(&project.join("target"), Scope::File, ""),
            target(&project.join("nothing"), Scope::Folder, ""),
            // Top-level path on a drive that does not exist: refused before any file access,
            // and harmless even if the check were broken.
            target(Path::new(r"Q:\ferrofs-test"), Scope::Contents, ""),
        ];
        let outcomes = execute(&targets, Disposal::Permanent, false, &log, None, |_, _| {});
        let reasons: Vec<String> = outcomes
            .iter()
            .map(|o| match o {
                Outcome::Skipped(reason) => reason.clone(),
                other => panic!("expected a skip, got {other:?}"),
            })
            .collect();
        assert!(reasons[0].contains("project file"));
        assert!(reasons[1].contains("not a file"));
        assert!(reasons[2].contains("not found"));
        assert!(reasons[3].contains("top-level"));
        assert!(project.join("target").exists());
    }
}
