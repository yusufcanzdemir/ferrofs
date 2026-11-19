//! Owns one index per NTFS volume, keeps them current from the USN journal and answers
//! IPC requests (SPEC §3.1).

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ferro_core::usn::{self, JournalInfo, JournalReader, UsnRecord};
use ferro_core::volume::VolumeInfo;
use ferro_core::{FoldOptions, Index, Query, SearchOptions, SortBy, search_volumes};
use ferro_ipc::{
    ErrorCode, Request, Response, Row, SearchRequest, SearchResponse, Sort, Stats, VolumeState,
    VolumeStatus,
};

/// Largest page a client may request.
const MAX_ROWS: u32 = 10_000;
/// How long one journal read waits before checking the stop flag.
const JOURNAL_WAIT: Duration = Duration::from_millis(500);
/// Pause before retrying after a volume error.
const RETRY_DELAY: Duration = Duration::from_secs(5);

enum State {
    Scanning,
    Ready { index: Box<Index>, next_usn: i64 },
    Failed(String),
}

struct Volume {
    info: VolumeInfo,
    state: RwLock<State>,
}

/// Shared by the pipe handlers (read) and the per-volume journal threads (write).
pub struct Engine {
    started: Instant,
    fold: FoldOptions,
    volumes: Vec<Volume>,
}

impl Engine {
    pub fn new(volumes: Vec<VolumeInfo>, fold: FoldOptions) -> Self {
        Self {
            started: Instant::now(),
            fold,
            volumes: volumes
                .into_iter()
                .map(|info| Volume {
                    info,
                    state: RwLock::new(State::Scanning),
                })
                .collect(),
        }
    }

    pub fn volume_count(&self) -> usize {
        self.volumes.len()
    }

    /// Scans volume `i`, then applies journal changes until `stop` is set. Errors are
    /// logged and retried; a journal reset triggers a fresh scan.
    pub fn run_volume(&self, i: usize, stop: &AtomicBool) {
        let volume = &self.volumes[i];
        let root = volume.info.root.as_str();
        while !stop.load(Ordering::Relaxed) {
            match self.scan_and_follow(volume, stop) {
                Ok(()) => return,
                Err(err) => {
                    tracing::error!(root, "volume indexing failed: {err:#}");
                    if !matches!(*self.read_state(volume), State::Ready { .. }) {
                        *self.write_state(volume) = State::Failed(format!("{err:#}"));
                    }
                    sleep_unless_stopped(RETRY_DELAY, stop);
                }
            }
        }
    }

    fn scan_and_follow(&self, volume: &Volume, stop: &AtomicBool) -> anyhow::Result<()> {
        let root = volume.info.root.as_str();
        let (index, journal) = usn::scan(root, self.fold)?;
        tracing::info!(root, entries = index.len(), "volume ready");
        *self.write_state(volume) = State::Ready {
            index: Box::new(index),
            next_usn: journal.next_usn,
        };
        self.follow(volume, journal, stop)
    }

    fn follow(
        &self,
        volume: &Volume,
        journal: JournalInfo,
        stop: &AtomicBool,
    ) -> anyhow::Result<()> {
        let mut reader = JournalReader::open(&volume.info.root, journal)?;
        let mut batch = RecordBatch::default();
        while !stop.load(Ordering::Relaxed) {
            batch.clear();
            let count = reader.read(JOURNAL_WAIT, |record| batch.push(record))?;
            if count == 0 {
                continue;
            }
            let mut state = self.write_state(volume);
            if let State::Ready { index, next_usn } = &mut *state {
                batch.apply(index);
                *next_usn = reader.next_usn();
                if index.needs_compaction() {
                    let started = Instant::now();
                    index.compact();
                    tracing::info!(
                        root = volume.info.root,
                        ms = started.elapsed().as_millis(),
                        "index compacted"
                    );
                }
            }
        }
        Ok(())
    }

    fn read_state<'a>(&self, volume: &'a Volume) -> std::sync::RwLockReadGuard<'a, State> {
        // A panic while holding the lock leaves the index usable; keep serving it.
        volume
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_state<'a>(&self, volume: &'a Volume) -> std::sync::RwLockWriteGuard<'a, State> {
        volume
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Answers one request (the handshake is handled by the pipe server).
    pub fn handle(&self, request: Request) -> Response {
        match request {
            Request::Search(search) => self.search(&search),
            Request::Stats => Response::Stats(self.stats()),
            Request::VolumeList => Response::Volumes(self.volume_statuses()),
            Request::Hello { .. } => error(ErrorCode::BadRequest, "duplicate Hello"),
        }
    }

    fn search(&self, request: &SearchRequest) -> Response {
        if self.volumes.is_empty() {
            return error(ErrorCode::NotReady, "no NTFS volume is available");
        }
        let query = match Query::parse(&request.query, self.fold) {
            Ok(query) => query,
            Err(err) => return error(ErrorCode::BadQuery, &err.to_string()),
        };
        let guards: Vec<_> = self.volumes.iter().map(|v| self.read_state(v)).collect();
        let indexes: Vec<&Index> = guards
            .iter()
            .filter_map(|state| match &**state {
                State::Ready { index, .. } => Some(&**index),
                _ => None,
            })
            .collect();
        let complete = indexes.len() == self.volumes.len();

        let options = SearchOptions {
            sort: match request.sort {
                Sort::Relevance => SortBy::Relevance,
                Sort::Name => SortBy::Name,
                Sort::Size => SortBy::Size,
                Sort::Modified => SortBy::Modified,
            },
            descending: request.descending,
            offset: request.offset as usize,
            limit: request.limit.min(MAX_ROWS) as usize,
        };
        let started = Instant::now();
        let result = search_volumes(&indexes, &query, &options);
        let took_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let rows = result
            .hits
            .iter()
            .map(|hit| {
                let index = indexes[hit.volume];
                Row {
                    path: index.path(hit.slot),
                    size: index.size(hit.slot),
                    mtime: index.mtime(hit.slot),
                    flags: index.flags(hit.slot),
                }
            })
            .collect();
        Response::Search(SearchResponse {
            rows,
            total: result.total as u64,
            took_us,
            complete,
        })
    }

    fn stats(&self) -> Stats {
        let (mut entries, mut memory) = (0u64, 0u64);
        for volume in &self.volumes {
            if let State::Ready { index, .. } = &*self.read_state(volume) {
                entries += index.len() as u64;
                memory += index.heap_bytes() as u64;
            }
        }
        Stats {
            volumes: u32::try_from(self.volumes.len()).unwrap_or(u32::MAX),
            entries,
            memory_bytes: memory,
            uptime_secs: self.started.elapsed().as_secs(),
        }
    }

    fn volume_statuses(&self) -> Vec<VolumeStatus> {
        self.volumes
            .iter()
            .map(|volume| {
                let state = self.read_state(volume);
                let (state, entries, next_usn) = match &*state {
                    State::Scanning => (VolumeState::Scanning, 0, 0),
                    State::Ready { index, next_usn } => {
                        (VolumeState::Ready, index.len() as u64, *next_usn)
                    }
                    State::Failed(reason) => (VolumeState::Failed(reason.clone()), 0, 0),
                };
                VolumeStatus {
                    root: volume.info.root.clone(),
                    guid_path: volume.info.guid_path.clone(),
                    state,
                    entries,
                    next_usn,
                }
            })
            .collect()
    }
}

fn error(code: ErrorCode, message: &str) -> Response {
    Response::Error {
        code,
        message: message.to_owned(),
    }
}

fn sleep_unless_stopped(duration: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Journal records copied out of the reader's buffer so the index lock is only held while
/// applying, not while waiting for changes. Reused between reads (no per-record allocation
/// once grown).
#[derive(Default)]
struct RecordBatch {
    records: Vec<(u64, u64, i64, u32, u32, usize, usize)>,
    names: Vec<u16>,
}

impl RecordBatch {
    fn clear(&mut self) {
        self.records.clear();
        self.names.clear();
    }

    fn push(&mut self, record: &UsnRecord<'_>) {
        let start = self.names.len();
        self.names.extend_from_slice(record.name);
        self.records.push((
            record.frn,
            record.parent_frn,
            record.usn,
            record.reason,
            record.attributes,
            start,
            record.name.len(),
        ));
    }

    fn apply(&self, index: &mut Index) {
        for &(frn, parent_frn, usn, reason, attributes, start, len) in &self.records {
            let record = UsnRecord {
                frn,
                parent_frn,
                usn,
                reason,
                attributes,
                name: &self.names[start..start + len],
            };
            usn::apply(index, &record);
        }
    }
}

#[cfg(test)]
mod tests {
    use ferro_core::{Name, NewEntry};

    use super::*;

    fn info(root: &str) -> VolumeInfo {
        VolumeInfo {
            root: root.into(),
            guid_path: format!(r"\\?\Volume{{{root}}}\"),
            filesystem: "NTFS".into(),
            label: String::new(),
            serial: 0,
        }
    }

    fn index(root: &str, names: &[&str]) -> Index {
        let mut index = Index::new(root, FoldOptions::default());
        for (i, name) in names.iter().enumerate() {
            index.upsert(&NewEntry {
                frn: 100 + i as u64,
                parent_frn: 5,
                name: Name::Utf8(name),
                attributes: 0x20,
                size: 0,
                alloc_size: 0,
                mtime: 0,
            });
        }
        index
    }

    /// Two volumes: C: ready, D: still scanning.
    fn engine() -> Engine {
        let engine = Engine::new(vec![info(r"C:\"), info(r"D:\")], FoldOptions::default());
        *engine.volumes[0].state.write().unwrap() = State::Ready {
            index: Box::new(index(
                r"C:\",
                &["report.pdf", "notes.txt", "Report 2024.docx"],
            )),
            next_usn: 77,
        };
        engine
    }

    #[test]
    fn search_over_ready_volumes() {
        let response = engine().handle(Request::Search(SearchRequest::new("report", 10)));
        let Response::Search(result) = response else {
            panic!("unexpected {response:?}");
        };
        assert_eq!(result.total, 2);
        assert!(!result.complete, "D: is still scanning");
        let paths: Vec<_> = result.rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, [r"C:\report.pdf", r"C:\Report 2024.docx"]);
    }

    #[test]
    fn bad_query_and_limits() {
        let engine = engine();
        let response = engine.handle(Request::Search(SearchRequest::new("size:>1mb", 10)));
        assert!(
            matches!(response, Response::Error { code: ErrorCode::BadQuery, message } if message.contains("not supported"))
        );

        let mut request = SearchRequest::new("", u32::MAX);
        request.sort = Sort::Name;
        let Response::Search(all) = engine.handle(Request::Search(request)) else {
            panic!("search failed");
        };
        assert_eq!(all.total, 3);
        assert_eq!(all.rows[0].path, r"C:\notes.txt");
    }

    #[test]
    fn stats_and_volume_list() {
        let engine = engine();
        let Response::Stats(stats) = engine.handle(Request::Stats) else {
            panic!("stats failed");
        };
        assert_eq!((stats.volumes, stats.entries), (2, 3));
        assert!(stats.memory_bytes > 0);

        let Response::Volumes(volumes) = engine.handle(Request::VolumeList) else {
            panic!("volume list failed");
        };
        assert_eq!(volumes[0].state, VolumeState::Ready);
        assert_eq!(volumes[0].next_usn, 77);
        assert_eq!(volumes[1].state, VolumeState::Scanning);
    }

    #[test]
    fn record_batch_applies_in_order() {
        let mut index = index(r"C:\", &["a.txt"]);
        let mut batch = RecordBatch::default();
        let rename_new: Vec<u16> = "b.txt".encode_utf16().collect();
        batch.push(&UsnRecord {
            frn: 100,
            parent_frn: 5,
            usn: 1,
            reason: 0x2000, // RENAME_NEW_NAME
            attributes: 0x20,
            name: &rename_new,
        });
        let created: Vec<u16> = "c.txt".encode_utf16().collect();
        batch.push(&UsnRecord {
            frn: 200,
            parent_frn: 5,
            usn: 2,
            reason: 0x100, // FILE_CREATE
            attributes: 0x20,
            name: &created,
        });
        batch.apply(&mut index);
        assert_eq!(index.path(index.lookup(100).unwrap()), r"C:\b.txt");
        assert_eq!(index.path(index.lookup(200).unwrap()), r"C:\c.txt");
    }
}
