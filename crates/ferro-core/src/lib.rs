//! MFT/USN reader, in-memory file index and query engine.

pub mod error;
pub mod fold;
pub mod index;
pub mod metadata;
pub mod mft;
pub mod query;
pub mod snapshot;
pub mod time;
pub mod usn;
pub mod volume;

pub use error::{Error, Result};
pub use fold::FoldOptions;
pub use index::{Index, Name, NewEntry};
pub use mft::scan_volume;
pub use query::{
    Hit, Query, QueryError, SearchOptions, SearchResult, SortBy, VolumesResult, search,
    search_volumes,
};
