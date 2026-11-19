//! MFT/USN reader, in-memory file index and query engine.

pub mod error;
pub mod fold;
pub mod index;
pub mod query;
pub mod usn;
pub mod volume;

pub use error::{Error, Result};
pub use fold::FoldOptions;
pub use index::{Index, Name, NewEntry};
pub use query::{
    Hit, Query, QueryError, SearchOptions, SearchResult, SortBy, VolumesResult, search,
    search_volumes,
};
