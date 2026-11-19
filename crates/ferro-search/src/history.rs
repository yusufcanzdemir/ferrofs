//! Launch history (`%LOCALAPPDATA%\ferrofs\history.bin`) used to boost frequently opened
//! results (SPEC §4).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ferro_ipc::Row;
use serde::{Deserialize, Serialize};

/// Entries kept; the least recently used are dropped beyond this.
const MAX_ENTRIES: usize = 5000;
/// How many positions one doubling of the launch count moves a row up.
const BOOST: f32 = 8.0;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Entry {
    count: u32,
    last_used: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    /// Keyed by the lowercase path.
    entries: HashMap<String, Entry>,
    #[serde(skip)]
    file: Option<PathBuf>,
}

impl History {
    pub fn default_file() -> Option<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(base).join("ferrofs").join("history.bin"))
    }

    /// Loads `file`; a missing or unreadable file yields an empty history.
    pub fn load(file: Option<PathBuf>) -> Self {
        let mut history = file
            .as_deref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|bytes| match postcard::from_bytes::<Self>(&bytes) {
                Ok(history) => Some(history),
                Err(err) => {
                    tracing::warn!("ignoring unreadable launch history: {err}");
                    None
                }
            })
            .unwrap_or_default();
        history.file = file;
        history
    }

    pub fn count(&self, path: &str) -> u32 {
        self.entries
            .get(&path.to_lowercase())
            .map_or(0, |e| e.count)
    }

    /// Counts a launch and saves the history.
    pub fn record(&mut self, path: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let entry = self.entries.entry(path.to_lowercase()).or_insert(Entry {
            count: 0,
            last_used: now,
        });
        entry.count = entry.count.saturating_add(1);
        entry.last_used = now;
        if self.entries.len() > MAX_ENTRIES {
            let mut by_age: Vec<_> = self
                .entries
                .iter()
                .map(|(k, e)| (e.last_used, k.clone()))
                .collect();
            by_age.sort_unstable();
            for (_, key) in by_age.into_iter().take(self.entries.len() - MAX_ENTRIES) {
                self.entries.remove(&key);
            }
        }
        if let Some(file) = &self.file
            && let Err(err) = self.save(file)
        {
            tracing::warn!("cannot save launch history: {err:#}");
        }
    }

    /// Writes to a temporary file first so a crash cannot leave a truncated history.
    fn save(&self, file: &Path) -> Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = postcard::to_allocvec(self).context("cannot encode history")?;
        let tmp = file.with_extension("bin.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, file)?;
        Ok(())
    }

    /// Moves frequently launched rows up within a relevance-sorted page. A row moves up by
    /// `BOOST * log2(1 + launches)` positions, so relevance still dominates.
    pub fn boost(&self, rows: &mut Vec<Row>) {
        if self.entries.is_empty() {
            return;
        }
        let mut scored: Vec<(f32, Row)> = rows
            .drain(..)
            .enumerate()
            .map(|(i, row)| {
                let launches = self.count(&row.path) as f32;
                (i as f32 - BOOST * (1.0 + launches).log2(), row)
            })
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        rows.extend(scored.into_iter().map(|(_, row)| row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str) -> Row {
        Row {
            path: path.into(),
            size: 0,
            mtime: 0,
            flags: 0,
        }
    }

    #[test]
    fn boost_moves_launched_rows_up_by_a_bounded_amount() {
        let mut history = History::default();
        for _ in 0..3 {
            history.record(r"C:\b\FAR.txt"); // 3 launches -> 16 positions up
        }
        let mut rows: Vec<Row> = (0..30).map(|i| row(&format!(r"C:\a\{i}.txt"))).collect();
        rows[20] = row(r"C:\b\far.txt");
        history.boost(&mut rows);
        let pos = rows.iter().position(|r| r.path == r"C:\b\far.txt").unwrap();
        // 20 - 16 = 4 ties with the row already at 4; ties keep the service's order.
        assert_eq!(pos, 5);
        assert_eq!(rows[0].path, r"C:\a\0.txt");
        assert_eq!(rows.len(), 30);
    }

    #[test]
    fn persists_and_survives_garbage() {
        let dir = std::env::temp_dir().join(format!("ferrofs-history-{}", std::process::id()));
        let file = dir.join("history.bin");
        let mut history = History::load(Some(file.clone()));
        history.record(r"C:\x.txt");
        history.record(r"C:\x.txt");
        assert_eq!(History::load(Some(file.clone())).count(r"c:\X.TXT"), 2);

        std::fs::write(&file, b"\xFF\xFF\xFF garbage").unwrap();
        assert_eq!(History::load(Some(file)).count(r"C:\x.txt"), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
