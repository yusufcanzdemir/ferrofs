//! Messages exchanged between the service and the UIs (SPEC §3.3).

use serde::{Deserialize, Serialize};

/// Bumped on every incompatible change; both sides must agree (checked in the handshake).
pub const PROTOCOL_VERSION: u32 = 4;

/// Sent by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Must be the first message on a connection.
    Hello {
        protocol: u32,
        client: String,
    },
    Search(SearchRequest),
    Stats,
    VolumeList,
    /// Folder sizes for the storage analyzer (SPEC §5.1).
    SizeTree(SizeTreeRequest),
    /// Files of equal size, the first step of the duplicate finder (SPEC §5.2).
    DuplicateCandidates(DuplicateCandidatesRequest),
    /// Developer artifacts (build output, dependency folders) for the cleanup scanner
    /// (SPEC §5.4).
    CleanupScan(CleanupScanRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub sort: Sort,
    pub descending: bool,
    pub offset: u32,
    pub limit: u32,
}

impl SearchRequest {
    /// Relevance-sorted first page.
    pub fn new(query: impl Into<String>, limit: u32) -> Self {
        Self {
            query: query.into(),
            sort: Sort::Relevance,
            descending: false,
            offset: 0,
            limit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeTreeRequest {
    /// Folder (or volume root such as `C:\`) to describe; empty = all volumes.
    pub root: String,
    /// Levels of children to include below `root` (0 = `root` only).
    pub depth: u8,
    /// Children listed per folder, largest first; the rest are summed in `SizeNode::rest`.
    pub max_children: u32,
    /// Query selecting the files that count, e.g. `ext:mp4;mkv`; empty = all files.
    /// Folders without matching files are left out.
    pub filter: String,
    /// Number of largest files below `root` to return.
    pub top_files: u32,
    /// Number of extensions (by bytes) below `root` to return.
    pub top_extensions: u32,
}

impl SizeTreeRequest {
    /// `depth` levels below `root`, up to 200 children each, no reports.
    pub fn new(root: impl Into<String>, depth: u8) -> Self {
        Self {
            root: root.into(),
            depth,
            max_children: 200,
            filter: String::new(),
            top_files: 0,
            top_extensions: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateCandidatesRequest {
    /// Folders to search (duplicates may span them); empty = all volumes.
    pub roots: Vec<String>,
    /// Smaller files are ignored.
    pub min_size: u64,
    /// Query selecting the files to consider, e.g. `ext:jpg;png`; empty = all files.
    pub filter: String,
    /// Most files to return; groups that do not fit are left out (largest waste first).
    pub max_files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupScanRequest {
    /// Folders to search; empty = all volumes.
    pub roots: Vec<String>,
    /// Also report Python virtual environments (`.venv` / `venv` with `pyvenv.cfg`).
    pub include_venv: bool,
    /// Smaller artifacts are left out.
    pub min_size: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sort {
    #[default]
    Relevance,
    Name,
    Size,
    Modified,
}

/// Sent by the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Hello {
        protocol: u32,
        service_version: String,
    },
    Search(SearchResponse),
    Stats(Stats),
    Volumes(Vec<VolumeStatus>),
    SizeTree(SizeTreeResponse),
    DuplicateCandidates(DuplicateCandidatesResponse),
    CleanupScan(CleanupScanResponse),
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub rows: Vec<Row>,
    /// Matches over all ready volumes (not just this page).
    pub total: u64,
    /// Time spent in the query engine.
    pub took_us: u64,
    /// False while some volumes are still being indexed (results may be incomplete).
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub path: String,
    pub size: u64,
    /// FILETIME.
    pub mtime: u64,
    /// `ferro_core::index::flags` bits (directory, hidden, …).
    pub flags: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeTreeResponse {
    /// Full path of `root` with the on-disk spelling (empty for "all volumes").
    pub path: String,
    pub root: SizeNode,
    /// Largest files below `root`, largest first.
    pub largest: Vec<Row>,
    /// Extensions with the most bytes below `root`.
    pub extensions: Vec<ExtensionRow>,
    pub took_us: u64,
    /// False while some volumes are still being indexed.
    pub complete: bool,
}

/// A file, folder or volume with the totals of everything below it. Hard links are
/// counted once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeNode {
    /// File or folder name; volume root path (`C:\`) for volumes; empty for "all volumes".
    pub name: String,
    /// Logical bytes.
    pub size: u64,
    /// Bytes allocated on disk (less than `size` when compressed or sparse).
    pub alloc_size: u64,
    pub files: u64,
    /// Sub-folders, recursively.
    pub folders: u64,
    /// Newest modification time below (FILETIME).
    pub mtime: u64,
    /// `ferro_core::index::flags` bits.
    pub flags: u16,
    /// Direct children, whether or not they are listed in `children`.
    pub child_count: u32,
    /// Largest children first; empty below the requested depth.
    pub children: Vec<SizeNode>,
    /// Children beyond `max_children`, summed.
    pub rest: Option<SizeRest>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeRest {
    pub count: u32,
    pub size: u64,
    pub alloc_size: u64,
    pub files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionRow {
    /// Lower-case extension without the dot; empty for files without one.
    pub ext: String,
    pub size: u64,
    pub alloc_size: u64,
    pub files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateCandidatesResponse {
    /// Groups of equal-size files, most wasted bytes (`size * (files - 1)`) first. Equal
    /// size does not mean equal content: the client hashes them.
    pub groups: Vec<SizeGroup>,
    /// Groups and files before `max_files` was applied.
    pub total_groups: u64,
    pub total_files: u64,
    pub took_us: u64,
    /// False while some volumes are still being indexed.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeGroup {
    pub size: u64,
    pub files: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupScanResponse {
    /// Largest first.
    pub artifacts: Vec<CleanupArtifact>,
    pub took_us: u64,
    /// False while some volumes are still being indexed.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupArtifact {
    pub kind: ArtifactKind,
    /// The artifact folder.
    pub path: String,
    pub size: u64,
    pub alloc_size: u64,
    pub files: u64,
    /// Newest modification time inside (FILETIME).
    pub mtime: u64,
    /// Marker file next to (or, for virtual environments, inside) the folder that made it
    /// an artifact; re-checked before deletion. Empty if none is needed.
    pub marker: String,
}

/// Mirrors `ferro_core::cleanup::ArtifactKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArtifactKind {
    NodeModules,
    RustTarget,
    MavenTarget,
    GradleCache,
    Build,
    Dist,
    PyCache,
    PythonVenv,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub volumes: u32,
    pub entries: u64,
    pub memory_bytes: u64,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub root: String,
    pub guid_path: String,
    pub state: VolumeState,
    pub entries: u64,
    pub next_usn: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolumeState {
    Scanning,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Client and service speak different protocol versions.
    ProtocolMismatch,
    /// The first message was not `Hello`, or a message was malformed.
    BadRequest,
    /// The query could not be parsed (message explains why).
    BadQuery,
    /// No volume is indexed yet.
    NotReady,
    /// The requested path is not in the index.
    NotFound,
    Internal,
}
