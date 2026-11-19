//! Messages exchanged between the service and the UIs (SPEC §3.3).

use serde::{Deserialize, Serialize};

/// Bumped on every incompatible change; both sides must agree (checked in the handshake).
pub const PROTOCOL_VERSION: u32 = 1;

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
    Internal,
}
