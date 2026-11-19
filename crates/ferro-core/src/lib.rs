//! MFT/USN reader, in-memory file index and query engine.

pub mod fold;
pub mod index;
pub mod query;

pub use fold::FoldOptions;
pub use index::{Index, Name, NewEntry};
pub use query::{Query, QueryError, SearchResult, search};
