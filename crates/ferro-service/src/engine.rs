//! Owns one index per NTFS volume, keeps them current from the USN journal and answers
//! IPC requests (SPEC §3.1).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use std::path::PathBuf;

use ferro_core::duplicates::{candidate_files, group_by_size};
use ferro_core::index::flags;
use ferro_core::metadata::{self, Metadata, MetadataReader};
use ferro_core::snapshot::{self, Header};
use ferro_core::usn::{self, JournalInfo, JournalReader, UsnRecord};
use ferro_core::volume::{self as core_volume, VolumeInfo};
use ferro_core::{
    FoldOptions, Index, Query, SearchOptions, SizeTotals, SortBy, match_mask, scan_volume,
    search_volumes,
};
use ferro_ipc::{
    DuplicateCandidatesRequest, DuplicateCandidatesResponse, ErrorCode, ExtensionRow, Request,
    Response, Row, SearchRequest, SearchResponse, SizeGroup, SizeNode, SizeRest, SizeTreeRequest,
    SizeTreeResponse, Sort, Stats, VolumeState, VolumeStatus,
};

/// Largest page a client may request.
const MAX_ROWS: u32 = 10_000;
/// How long one journal read waits before checking the stop flag.
const JOURNAL_WAIT: Duration = Duration::from_millis(500);
/// Pause before retrying after a volume error.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// Snapshots are rewritten at most this often while the index keeps changing.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Most nodes in one size tree response.
const MAX_TREE_NODES: usize = 50_000;
/// Most children listed per folder in a size tree.
const MAX_TREE_CHILDREN: u32 = 5_000;
/// Most largest-file and extension rows in a size tree response.
const MAX_TREE_REPORT: u32 = 1_000;
/// Cached size totals are dropped after this long without a request.
const TOTALS_IDLE: Duration = Duration::from_secs(60);
/// Most files in one duplicate candidates response.
const MAX_DUPLICATE_FILES: u32 = 200_000;

#[derive(Debug)]
enum State {
    Scanning,
    Ready { index: Box<Index>, next_usn: i64 },
    Failed(String),
}

#[derive(Debug)]
struct Volume {
    info: VolumeInfo,
    state: RwLock<State>,
    /// Bumped (under the state write lock) whenever the index changes.
    generation: AtomicU64,
    /// Size totals of the last analyzer request, reused while the index is unchanged.
    totals: Mutex<Option<CachedTotals>>,
}

#[derive(Debug)]
struct CachedTotals {
    generation: u64,
    filter: String,
    totals: Arc<SizeTotals>,
    last_used: Instant,
}

/// Shared by the pipe handlers (read) and the per-volume journal threads (write).
#[derive(Debug)]
pub struct Engine {
    started: Instant,
    fold: FoldOptions,
    volumes: Vec<Volume>,
    /// Folder for index snapshots; `None` disables them.
    snapshot_dir: Option<PathBuf>,
}

impl Engine {
    pub fn new(volumes: Vec<VolumeInfo>, fold: FoldOptions, snapshot_dir: Option<PathBuf>) -> Self {
        Self {
            started: Instant::now(),
            fold,
            snapshot_dir,
            volumes: volumes
                .into_iter()
                .map(|info| Volume {
                    info,
                    state: RwLock::new(State::Scanning),
                    generation: AtomicU64::new(0),
                    totals: Mutex::new(None),
                })
                .collect(),
        }
    }

    /// Engine over indexes that are already built, without journal threads or snapshots
    /// (development stand-in and tests).
    pub fn with_indexes(volumes: Vec<(VolumeInfo, Index)>, fold: FoldOptions) -> Self {
        let (infos, indexes): (Vec<_>, Vec<_>) = volumes.into_iter().unzip();
        let engine = Self::new(infos, fold, None);
        for (volume, index) in engine.volumes.iter().zip(indexes) {
            *volume
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = State::Ready {
                index: Box::new(index),
                next_usn: 0,
            };
        }
        engine
    }

    pub fn volume_count(&self) -> usize {
        self.volumes.len()
    }

    /// Scans volume `i`, then applies journal changes until `stop` is set. Errors are
    /// logged and retried; a journal reset triggers a fresh scan.
    pub fn run_volume(&self, i: usize, stop: &AtomicBool) {
        let volume = &self.volumes[i];
        let root = volume.info.root.as_str();
        let mut try_snapshot = true;
        while !stop.load(Ordering::Relaxed) {
            let result = self.scan_and_follow(volume, stop, std::mem::take(&mut try_snapshot));
            match result {
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

    fn snapshot_path(&self, volume: &Volume) -> Option<PathBuf> {
        // \\?\Volume{GUID}\ -> Volume{GUID}.bin
        let guid: String = volume
            .info
            .guid_path
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '{' | '}' | '-'))
            .collect();
        Some(self.snapshot_dir.as_ref()?.join(format!("{guid}.bin")))
    }

    /// Loads the snapshot if it belongs to this volume and its journal position is still
    /// available; otherwise `None` (a full scan follows).
    fn load_snapshot(&self, volume: &Volume) -> Option<(Index, JournalInfo)> {
        let path = self.snapshot_path(volume)?;
        if !path.exists() {
            return None;
        }
        let started = Instant::now();
        let current = core_volume::Volume::open(&volume.info.root)
            .and_then(|v| usn::query_journal(&v))
            .ok()?;
        let header = snapshot::read_header(&path)
            .map_err(|err| tracing::warn!("ignoring snapshot: {err}"))
            .ok()?;
        if header.volume_guid != volume.info.guid_path
            || header.journal_id != current.journal_id
            || header.next_usn < current.first_usn
            || header.next_usn > current.next_usn
        {
            tracing::info!(root = volume.info.root, "snapshot is stale; rescanning");
            return None;
        }
        let (_, index) = snapshot::load(&path)
            .map_err(|err| tracing::warn!("ignoring snapshot: {err}"))
            .ok()?;
        if index.fold_options() != self.fold {
            return None;
        }
        tracing::info!(
            root = volume.info.root,
            entries = index.len(),
            ms = started.elapsed().as_millis(),
            "snapshot loaded"
        );
        Some((
            index,
            JournalInfo {
                next_usn: header.next_usn,
                ..current
            },
        ))
    }

    fn save_snapshot(&self, volume: &Volume, journal_id: u64) {
        let Some(path) = self.snapshot_path(volume) else {
            return;
        };
        let started = Instant::now();
        let state = self.read_state(volume);
        let State::Ready { index, next_usn } = &*state else {
            return;
        };
        let header = Header {
            format: snapshot::FORMAT,
            volume_guid: volume.info.guid_path.clone(),
            journal_id,
            next_usn: *next_usn,
        };
        match snapshot::save(&path, &header, index) {
            Ok(()) => tracing::info!(
                root = volume.info.root,
                ms = started.elapsed().as_millis(),
                "snapshot saved"
            ),
            Err(err) => tracing::warn!(root = volume.info.root, "cannot save snapshot: {err}"),
        }
    }

    fn scan_and_follow(
        &self,
        volume: &Volume,
        stop: &AtomicBool,
        try_snapshot: bool,
    ) -> anyhow::Result<()> {
        let root = volume.info.root.as_str();
        let (index, journal, from_snapshot) =
            match try_snapshot.then(|| self.load_snapshot(volume)).flatten() {
                Some((index, journal)) => (index, journal, true),
                None => {
                    let (index, journal) = scan_volume(root, self.fold)?;
                    (index, journal, false)
                }
            };
        tracing::info!(root, entries = index.len(), from_snapshot, "volume ready");
        {
            let mut state = self.write_state(volume);
            *state = State::Ready {
                index: Box::new(index),
                next_usn: journal.next_usn,
            };
            volume.generation.fetch_add(1, Ordering::Release);
        }
        if !from_snapshot {
            self.save_snapshot(volume, journal.journal_id);
        }
        let result = self.follow(volume, journal, stop);
        self.save_snapshot(volume, journal.journal_id);
        result
    }

    fn follow(
        &self,
        volume: &Volume,
        journal: JournalInfo,
        stop: &AtomicBool,
    ) -> anyhow::Result<()> {
        let mut reader = JournalReader::open(&volume.info.root, journal)?;
        let metadata = MetadataReader::open(&volume.info.root)
            .map_err(|err| tracing::warn!("sizes will not be refreshed: {err}"))
            .ok();
        let mut batch = RecordBatch::default();
        let mut refreshed: Vec<(u64, Metadata)> = Vec::new();
        let mut last_snapshot = Instant::now();
        let mut dirty = false;
        while !stop.load(Ordering::Relaxed) {
            if dirty && last_snapshot.elapsed() >= SNAPSHOT_INTERVAL {
                self.save_snapshot(volume, journal.journal_id);
                (dirty, last_snapshot) = (false, Instant::now());
            }
            expire_totals(volume);
            batch.clear();
            let count = reader.read(JOURNAL_WAIT, |record| batch.push(record))?;
            if count == 0 {
                continue;
            }
            // Read new sizes and times before taking the lock, so searches never wait on
            // file system calls.
            refreshed.clear();
            if let Some(metadata) = &metadata {
                refreshed.extend(
                    batch
                        .refresh_targets()
                        .into_iter()
                        .filter_map(|frn| Some((frn, metadata.query(frn)?))),
                );
            }
            let mut state = self.write_state(volume);
            if let State::Ready { index, next_usn } = &mut *state {
                batch.apply(index);
                for (frn, m) in &refreshed {
                    index.set_metadata(*frn, m.size, m.alloc_size, m.mtime, m.attributes);
                }
                *next_usn = reader.next_usn();
                volume.generation.fetch_add(1, Ordering::Release);
                dirty = true;
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
            Request::SizeTree(request) => self.size_tree(&request),
            Request::DuplicateCandidates(request) => self.duplicate_candidates(&request),
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

    /// Size totals of `volume` for `filter`, from the cache when the index is unchanged.
    /// The caller holds the state read lock, so the index cannot change meanwhile.
    fn size_totals(
        &self,
        volume: &Volume,
        index: &Index,
        filter: &str,
        query: Option<&Query>,
    ) -> Arc<SizeTotals> {
        let generation = volume.generation.load(Ordering::Acquire);
        let mut cache = volume
            .totals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.as_mut()
            && cached.generation == generation
            && cached.filter == filter
        {
            cached.last_used = Instant::now();
            return Arc::clone(&cached.totals);
        }
        *cache = None; // free the old totals before building new ones
        let started = Instant::now();
        let totals = Arc::new(SizeTotals::build(
            index,
            query.map(|query| match_mask(index, query)),
        ));
        tracing::debug!(
            root = volume.info.root,
            ms = started.elapsed().as_millis(),
            mb = totals.heap_bytes() >> 20,
            "size totals built"
        );
        *cache = Some(CachedTotals {
            generation,
            filter: filter.to_owned(),
            totals: Arc::clone(&totals),
            last_used: Instant::now(),
        });
        totals
    }

    fn size_tree(&self, request: &SizeTreeRequest) -> Response {
        if self.volumes.is_empty() {
            return error(ErrorCode::NotReady, "no NTFS volume is available");
        }
        let filter = request.filter.trim();
        let query = if filter.is_empty() {
            None
        } else {
            match Query::parse(filter, self.fold) {
                Ok(query) => Some(query),
                Err(err) => return error(ErrorCode::BadQuery, &err.to_string()),
            }
        };
        let started = Instant::now();
        let guards: Vec<_> = self.volumes.iter().map(|v| self.read_state(v)).collect();
        let ready: Vec<(&Volume, &Index)> = self
            .volumes
            .iter()
            .zip(&guards)
            .filter_map(|(volume, state)| match &**state {
                State::Ready { index, .. } => Some((volume, &**index)),
                _ => None,
            })
            .collect();
        let complete = ready.len() == self.volumes.len();
        let mut shape = TreeShape {
            max_children: request.max_children.clamp(1, MAX_TREE_CHILDREN) as usize,
            budget: MAX_TREE_NODES,
        };
        let top_files = request.top_files.min(MAX_TREE_REPORT) as usize;
        let top_extensions = request.top_extensions.min(MAX_TREE_REPORT) as usize;

        let root = request.root.trim();
        let (path, node, largest, extensions) = if root.is_empty() {
            // All volumes: one child per ready volume.
            let mut node = SizeNode {
                flags: flags::DIR,
                child_count: u32::try_from(ready.len()).unwrap_or(u32::MAX),
                ..SizeNode::default()
            };
            let mut largest: Vec<Row> = Vec::new();
            let mut extensions: Vec<ExtensionRow> = Vec::new();
            for &(volume, index) in &ready {
                let totals = self.size_totals(volume, index, filter, query.as_ref());
                let depth = request.depth.saturating_sub(1);
                let child = size_node(index, &totals, None, depth, &mut shape);
                node.size += child.size;
                node.alloc_size += child.alloc_size;
                node.files += child.files;
                node.folders += child.folders;
                node.mtime = node.mtime.max(child.mtime);
                if request.depth > 0 {
                    node.children.push(child);
                }
                largest.extend(largest_rows(index, &totals, None, top_files));
                let rows = extension_rows(index, &totals, None, top_extensions);
                merge_extensions(&mut extensions, rows);
            }
            node.children.sort_by_key(|n| std::cmp::Reverse(n.size));
            largest.sort_by_key(|r| std::cmp::Reverse(r.size));
            largest.truncate(top_files);
            extensions.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.ext.cmp(&b.ext)));
            extensions.truncate(top_extensions);
            (String::new(), node, largest, extensions)
        } else {
            let Some(volume) = self.volume_for(root) else {
                let message = format!("{root} is not on an indexed volume");
                return error(ErrorCode::NotFound, &message);
            };
            let Some(&(_, index)) = ready.iter().find(|(v, _)| std::ptr::eq(*v, volume)) else {
                let message = format!("{} is still being indexed", volume.info.root);
                return error(ErrorCode::NotReady, &message);
            };
            let relative = root.get(volume.info.root.len()..).unwrap_or_default();
            let totals = self.size_totals(volume, index, filter, query.as_ref());
            let Some(folder) = totals.resolve(index, relative) else {
                return error(ErrorCode::NotFound, &format!("{root} was not found"));
            };
            let path = folder.map_or_else(|| index.volume_root().to_owned(), |s| index.path(s));
            let node = size_node(index, &totals, folder, request.depth, &mut shape);
            let largest = largest_rows(index, &totals, folder, top_files);
            let extensions = extension_rows(index, &totals, folder, top_extensions);
            (path, node, largest, extensions)
        };
        Response::SizeTree(SizeTreeResponse {
            path,
            root: node,
            largest,
            extensions,
            took_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            complete,
        })
    }

    fn duplicate_candidates(&self, request: &DuplicateCandidatesRequest) -> Response {
        if self.volumes.is_empty() {
            return error(ErrorCode::NotReady, "no NTFS volume is available");
        }
        let filter = request.filter.trim();
        let query = if filter.is_empty() {
            None
        } else {
            match Query::parse(filter, self.fold) {
                Ok(query) => Some(query),
                Err(err) => return error(ErrorCode::BadQuery, &err.to_string()),
            }
        };
        let started = Instant::now();
        let guards: Vec<_> = self.volumes.iter().map(|v| self.read_state(v)).collect();
        let ready: Vec<(&Volume, &Index)> = self
            .volumes
            .iter()
            .zip(&guards)
            .filter_map(|(volume, state)| match &**state {
                State::Ready { index, .. } => Some((volume, &**index)),
                _ => None,
            })
            .collect();
        let complete = ready.len() == self.volumes.len();
        let totals: Vec<Arc<SizeTotals>> = ready
            .iter()
            .map(|&(volume, index)| self.size_totals(volume, index, filter, query.as_ref()))
            .collect();

        // (position in `ready`, folder) to search.
        let mut targets: Vec<(usize, Option<u32>)> = Vec::new();
        if request.roots.iter().all(|root| root.trim().is_empty()) {
            targets.extend((0..ready.len()).map(|i| (i, None)));
        }
        for root in request
            .roots
            .iter()
            .map(|r| r.trim())
            .filter(|r| !r.is_empty())
        {
            let Some(volume) = self.volume_for(root) else {
                let message = format!("{root} is not on an indexed volume");
                return error(ErrorCode::NotFound, &message);
            };
            let Some(i) = ready.iter().position(|(v, _)| std::ptr::eq(*v, volume)) else {
                let message = format!("{} is still being indexed", volume.info.root);
                return error(ErrorCode::NotReady, &message);
            };
            let relative = root.get(volume.info.root.len()..).unwrap_or_default();
            let Some(folder) = totals[i].resolve(ready[i].1, relative) else {
                return error(ErrorCode::NotFound, &format!("{root} was not found"));
            };
            targets.push((i, folder));
        }

        let mut files: Vec<(u64, (u32, u32))> = Vec::new();
        for (i, folder) in targets {
            let volume = u32::try_from(i).unwrap_or(u32::MAX);
            let found = candidate_files(ready[i].1, &totals[i], folder, request.min_size);
            files.extend(found.into_iter().map(|(size, slot)| (size, (volume, slot))));
        }
        let mut groups = group_by_size(files);
        groups.sort_by_key(|(size, members)| {
            std::cmp::Reverse(size.saturating_mul(members.len() as u64 - 1))
        });
        let total_groups = groups.len() as u64;
        let total_files = groups.iter().map(|(_, m)| m.len() as u64).sum();

        let budget = request.max_files.min(MAX_DUPLICATE_FILES) as usize;
        let mut used = 0usize;
        let mut out = Vec::new();
        for (size, members) in groups {
            if used + members.len() > budget {
                continue;
            }
            used += members.len();
            let files = members
                .into_iter()
                .map(|(volume, slot)| {
                    let index = ready[volume as usize].1;
                    Row {
                        path: index.path(slot),
                        size: index.size(slot),
                        mtime: index.mtime(slot),
                        flags: index.flags(slot),
                    }
                })
                .collect();
            out.push(SizeGroup { size, files });
        }
        Response::DuplicateCandidates(DuplicateCandidatesResponse {
            groups: out,
            total_groups,
            total_files,
            took_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            complete,
        })
    }

    /// The volume whose root (`C:\`) starts `path`, ignoring case; `C:` alone matches too.
    fn volume_for(&self, path: &str) -> Option<&Volume> {
        self.volumes.iter().find(|volume| {
            let root = volume.info.root.as_str();
            path.get(..root.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(root))
                || path.eq_ignore_ascii_case(root.trim_end_matches('\\'))
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

/// Drops the cached size totals of `volume` once no analyzer has asked for a while.
fn expire_totals(volume: &Volume) {
    if let Ok(mut cache) = volume.totals.try_lock()
        && cache
            .as_ref()
            .is_some_and(|cached| cached.last_used.elapsed() >= TOTALS_IDLE)
    {
        *cache = None;
    }
}

/// Limits for one size tree response.
struct TreeShape {
    max_children: usize,
    /// Nodes that may still be added.
    budget: usize,
}

/// Node for `folder` (`None` = volume root) with `depth` levels of children.
fn size_node(
    index: &Index,
    totals: &SizeTotals,
    folder: Option<u32>,
    depth: u8,
    shape: &mut TreeShape,
) -> SizeNode {
    let own = totals.totals_of(index, folder);
    let mut node = SizeNode {
        name: folder.map_or_else(
            || index.volume_root().to_owned(),
            |s| index.name(s).to_owned(),
        ),
        size: own.size,
        alloc_size: own.alloc_size,
        files: own.files,
        folders: own.folders,
        mtime: own.mtime,
        flags: folder.map_or(flags::DIR, |s| index.flags(s)),
        ..SizeNode::default()
    };
    if depth == 0 || shape.budget == 0 {
        node.child_count = u32::try_from(totals.child_count(index, folder)).unwrap_or(u32::MAX);
        return node;
    }
    let children = totals.children(index, folder);
    node.child_count = u32::try_from(children.len()).unwrap_or(u32::MAX);
    let shown = children.len().min(shape.max_children).min(shape.budget);
    shape.budget -= shown;
    node.children = children[..shown]
        .iter()
        .map(|&child| size_node(index, totals, Some(child), depth - 1, shape))
        .collect();
    if children.len() > shown {
        let mut rest = SizeRest {
            count: u32::try_from(children.len() - shown).unwrap_or(u32::MAX),
            ..SizeRest::default()
        };
        for &child in &children[shown..] {
            let t = totals.totals(index, child);
            rest.size += t.size;
            rest.alloc_size += t.alloc_size;
            rest.files += t.files;
        }
        node.rest = Some(rest);
    }
    node
}

fn largest_rows(index: &Index, totals: &SizeTotals, folder: Option<u32>, n: usize) -> Vec<Row> {
    totals
        .largest_files(index, folder, n)
        .into_iter()
        .map(|slot| Row {
            path: index.path(slot),
            size: index.size(slot),
            mtime: index.mtime(slot),
            flags: index.flags(slot),
        })
        .collect()
}

fn extension_rows(
    index: &Index,
    totals: &SizeTotals,
    folder: Option<u32>,
    n: usize,
) -> Vec<ExtensionRow> {
    totals
        .extensions(index, folder, n)
        .into_iter()
        .map(|e| ExtensionRow {
            ext: e.ext,
            size: e.size,
            alloc_size: e.alloc_size,
            files: e.files,
        })
        .collect()
}

/// Adds the rows of another volume, summing equal extensions.
fn merge_extensions(into: &mut Vec<ExtensionRow>, rows: Vec<ExtensionRow>) {
    for row in rows {
        match into.iter_mut().find(|e| e.ext == row.ext) {
            Some(existing) => {
                existing.size += row.size;
                existing.alloc_size += row.alloc_size;
                existing.files += row.files;
            }
            None => into.push(row),
        }
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

    /// Files whose size or time should be re-read after this batch.
    fn refresh_targets(&self) -> Vec<u64> {
        let mut frns: Vec<u64> = self
            .records
            .iter()
            .filter(|r| metadata::needs_refresh(r.3))
            .map(|r| r.0)
            .collect();
        frns.sort_unstable();
        frns.dedup();
        frns
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
        let engine = Engine::new(
            vec![info(r"C:\"), info(r"D:\")],
            FoldOptions::default(),
            None,
        );
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
        let response = engine.handle(Request::Search(SearchRequest::new("dc:today", 10)));
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

    /// C:\Games\big.pak (5000), C:\Games\save.dat (10), C:\Docs\a.pdf (300) and
    /// C:\Docs\b.pdf (200); D: still scanning.
    fn tree_engine() -> Engine {
        let engine = Engine::new(
            vec![info(r"C:\"), info(r"D:\")],
            FoldOptions::default(),
            None,
        );
        let mut index = Index::new(r"C:\", FoldOptions::default());
        for (frn, parent, name, dir, size) in [
            (20, 5, "Games", true, 0),
            (21, 20, "big.pak", false, 5000),
            (22, 20, "save.dat", false, 10),
            (30, 5, "Docs", true, 0),
            (31, 30, "a.pdf", false, 300),
            (32, 30, "b.pdf", false, 200),
        ] {
            index.upsert(&NewEntry {
                frn,
                parent_frn: parent,
                name: Name::Utf8(name),
                attributes: if dir { 0x10 } else { 0x20 },
                size,
                alloc_size: size,
                mtime: frn,
            });
        }
        *engine.volumes[0].state.write().unwrap() = State::Ready {
            index: Box::new(index),
            next_usn: 1,
        };
        engine
    }

    fn size_tree(engine: &Engine, request: SizeTreeRequest) -> SizeTreeResponse {
        match engine.handle(Request::SizeTree(request)) {
            Response::SizeTree(tree) => tree,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn size_tree_of_all_volumes_and_a_folder() {
        let engine = tree_engine();
        let mut request = SizeTreeRequest::new("", 2);
        request.top_files = 2;
        request.top_extensions = 5;
        let tree = size_tree(&engine, request);
        assert!(!tree.complete);
        assert_eq!((tree.root.size, tree.root.files), (5510, 4));
        assert_eq!(tree.root.children.len(), 1);
        let c = &tree.root.children[0];
        assert_eq!(c.name, r"C:\");
        let names: Vec<_> = c.children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["Games", "Docs"]);
        assert!(c.children[0].children.is_empty(), "depth limit");
        assert_eq!(c.children[0].child_count, 2);
        let largest: Vec<_> = tree.largest.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(largest, [r"C:\Games\big.pak", r"C:\Docs\a.pdf"]);
        assert_eq!(tree.extensions[0].ext, "pak");
        assert_eq!(tree.extensions[1].ext, "pdf");
        assert_eq!(tree.extensions[1].files, 2);

        // A folder, case-insensitive, with a child limit and a filter.
        let mut request = SizeTreeRequest::new(r"c:\docs", 1);
        request.max_children = 1;
        let tree = size_tree(&engine, request);
        assert_eq!(tree.path, r"C:\Docs");
        assert_eq!(tree.root.children[0].name, "a.pdf");
        assert_eq!(
            tree.root.rest.as_ref().map(|r| (r.count, r.size)),
            Some((1, 200))
        );

        let mut request = SizeTreeRequest::new("C:", 1);
        request.filter = "ext:pdf".into();
        let tree = size_tree(&engine, request);
        assert_eq!(tree.path, r"C:\");
        assert_eq!(tree.root.size, 500);
        assert_eq!(tree.root.children.len(), 1);
    }

    #[test]
    fn size_tree_errors_and_cache() {
        let engine = tree_engine();
        let response = engine.handle(Request::SizeTree(SizeTreeRequest::new(r"C:\nope", 1)));
        assert!(matches!(
            response,
            Response::Error {
                code: ErrorCode::NotFound,
                ..
            }
        ));
        let response = engine.handle(Request::SizeTree(SizeTreeRequest::new(r"D:\x", 1)));
        assert!(matches!(
            response,
            Response::Error {
                code: ErrorCode::NotReady,
                ..
            }
        ));
        let response = engine.handle(Request::SizeTree(SizeTreeRequest::new(r"Q:\", 1)));
        assert!(matches!(
            response,
            Response::Error {
                code: ErrorCode::NotFound,
                ..
            }
        ));
        let mut request = SizeTreeRequest::new("", 1);
        request.filter = "size:>>".into();
        let response = engine.handle(Request::SizeTree(request));
        assert!(matches!(
            response,
            Response::Error {
                code: ErrorCode::BadQuery,
                ..
            }
        ));

        // Same generation and filter: the cached totals are reused.
        size_tree(&engine, SizeTreeRequest::new(r"C:\", 1));
        let first = engine.volumes[0]
            .totals
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| (c.generation, Arc::as_ptr(&c.totals)));
        size_tree(&engine, SizeTreeRequest::new(r"C:\Games", 1));
        let second = engine.volumes[0]
            .totals
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| (c.generation, Arc::as_ptr(&c.totals)));
        assert_eq!(first, second);
        engine.volumes[0].generation.fetch_add(1, Ordering::Release);
        size_tree(&engine, SizeTreeRequest::new(r"C:\Games", 1));
        let third = engine.volumes[0]
            .totals
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| (c.generation, Arc::as_ptr(&c.totals)));
        assert_ne!(first.map(|c| c.0), third.map(|c| c.0));
    }

    /// C: with a\x.iso, a\y.iso, b\z.iso (3000 each), a\p.jpg, b\q.jpg (500 each) and a
    /// lone 7000-byte file; D: with d.iso (3000).
    fn duplicate_engine() -> Engine {
        let mut c = Index::new(r"C:\", FoldOptions::default());
        let mut d = Index::new(r"D:\", FoldOptions::default());
        for (on_d, frn, parent, name, dir, size) in [
            (false, 20, 5, "a", true, 0),
            (false, 21, 20, "x.iso", false, 3000),
            (false, 22, 20, "y.iso", false, 3000),
            (false, 23, 20, "p.jpg", false, 500),
            (false, 30, 5, "b", true, 0),
            (false, 31, 30, "z.iso", false, 3000),
            (false, 32, 30, "q.jpg", false, 500),
            (false, 33, 30, "lone.bin", false, 7000),
            (true, 20, 5, "d.iso", false, 3000),
        ] {
            let index = if on_d { &mut d } else { &mut c };
            index.upsert(&NewEntry {
                frn,
                parent_frn: parent,
                name: Name::Utf8(name),
                attributes: if dir { 0x10 } else { 0x20 },
                size,
                alloc_size: size,
                mtime: frn,
            });
        }
        Engine::with_indexes(
            vec![(info(r"C:\"), c), (info(r"D:\"), d)],
            FoldOptions::default(),
        )
    }

    fn candidates(engine: &Engine, roots: &[&str], filter: &str, max_files: u32) -> Response {
        engine.handle(Request::DuplicateCandidates(DuplicateCandidatesRequest {
            roots: roots.iter().map(|r| (*r).to_owned()).collect(),
            min_size: 100,
            filter: filter.into(),
            max_files,
        }))
    }

    #[test]
    fn duplicate_candidates_across_volumes_and_roots() {
        let engine = duplicate_engine();
        let Response::DuplicateCandidates(all) = candidates(&engine, &[], "", 100) else {
            panic!("candidates failed");
        };
        assert!(all.complete);
        assert_eq!((all.total_groups, all.total_files), (2, 6));
        // Most wasted bytes first: 3 * 3000 > 1 * 500.
        assert_eq!(all.groups[0].size, 3000);
        let paths: Vec<_> = all.groups[0]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert!(paths.contains(&r"D:\d.iso"), "{paths:?}");
        assert_eq!(all.groups[1].files.len(), 2);

        let Response::DuplicateCandidates(a) = candidates(&engine, &[r"c:\A"], "", 100) else {
            panic!("candidates failed");
        };
        assert_eq!(a.groups.len(), 1, "p.jpg has no twin inside C:\\a");
        assert_eq!(a.groups[0].files.len(), 2);

        // Overlapping roots do not list a file twice; the filter applies.
        let Response::DuplicateCandidates(iso) =
            candidates(&engine, &[r"C:\", r"C:\a"], "ext:iso", 100)
        else {
            panic!("candidates failed");
        };
        assert_eq!((iso.total_groups, iso.total_files), (1, 3));

        // The file budget drops groups that do not fit, keeping smaller ones.
        let Response::DuplicateCandidates(small) = candidates(&engine, &[], "", 3) else {
            panic!("candidates failed");
        };
        assert_eq!(small.groups.len(), 1);
        assert_eq!(small.groups[0].size, 500);
        assert_eq!(small.total_files, 6);

        let missing = candidates(&engine, &[r"C:\nope"], "", 100);
        assert!(matches!(
            missing,
            Response::Error {
                code: ErrorCode::NotFound,
                ..
            }
        ));
    }
}
