//! Index snapshots (SPEC §2.5): saved on shutdown and periodically, loaded at startup so
//! only the journal since the snapshot has to be replayed.
//!
//! Layout: `FERROIDX` magic, postcard [`Header`], postcard [`Index`].

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::index::Index;

const MAGIC: &[u8; 8] = b"FERROIDX";
/// Bumped whenever the header or the index layout changes.
pub const FORMAT: u32 = 1;

/// Identifies the volume and the journal position the snapshot is valid for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub format: u32,
    pub volume_guid: String,
    pub journal_id: u64,
    /// First journal record not yet reflected in the index.
    pub next_usn: i64,
}

fn snapshot_error(err: impl std::fmt::Display) -> Error {
    Error::Snapshot(err.to_string())
}

/// Writes the snapshot to a temporary file and renames it over `path`, so a crash never
/// leaves a truncated snapshot behind.
pub fn save(path: &Path, header: &Header, index: &Index) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let file = File::create(&tmp).map_err(snapshot_error)?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);
    writer.write_all(MAGIC).map_err(snapshot_error)?;
    let writer = postcard::to_io(header, writer).map_err(snapshot_error)?;
    let mut writer = postcard::to_io(index, writer).map_err(snapshot_error)?;
    writer.flush().map_err(snapshot_error)?;
    drop(writer);
    std::fs::rename(&tmp, path).map_err(snapshot_error)
}

/// Reads only the header (cheap) to decide whether loading is worthwhile.
pub fn read_header(path: &Path) -> Result<Header> {
    use std::io::Read;
    let mut prefix = vec![0u8; 4096];
    let mut file = File::open(path).map_err(snapshot_error)?;
    let n = file.read(&mut prefix).map_err(snapshot_error)?;
    parse_header(&prefix[..n]).map(|(header, _)| header)
}

fn parse_header(bytes: &[u8]) -> Result<(Header, &[u8])> {
    let rest = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or_else(|| snapshot_error("not a ferrofs snapshot"))?;
    let (header, rest): (Header, _) = postcard::take_from_bytes(rest).map_err(snapshot_error)?;
    if header.format != FORMAT {
        return Err(snapshot_error(format!(
            "snapshot format {} (expected {FORMAT})",
            header.format
        )));
    }
    Ok((header, rest))
}

/// Loads a snapshot written by [`save`].
pub fn load(path: &Path) -> Result<(Header, Index)> {
    let bytes = std::fs::read(path).map_err(snapshot_error)?;
    let (header, rest) = parse_header(&bytes)?;
    let mut index: Index = postcard::from_bytes(rest).map_err(snapshot_error)?;
    index.shrink_to_fit();
    Ok((header, index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    #[test]
    fn roundtrip_and_rejections() {
        let dir = std::env::temp_dir().join(format!("ferrofs-snapshot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("C.bin");
        let header = Header {
            format: FORMAT,
            volume_guid: r"\\?\Volume{1234}\".into(),
            journal_id: 42,
            next_usn: 9000,
        };
        let index = sample();
        save(&path, &header, &index).unwrap();
        assert!(!path.with_extension("tmp").exists());

        assert_eq!(read_header(&path).unwrap(), header);
        let (loaded_header, loaded) = load(&path).unwrap();
        assert_eq!(loaded_header, header);
        assert_eq!(loaded.len(), index.len());
        assert!(loaded.heap_bytes() <= index.heap_bytes());
        let slot = loaded.lookup(30).unwrap();
        assert_eq!(loaded.path(slot), r"C:\Users\me\Docs\Report.PDF");
        assert_eq!(loaded.folded_name(slot), b"report.pdf");

        // Truncated, foreign and future-format files are refused, not misread.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        assert!(load(&path).is_err());
        std::fs::write(&path, b"something else entirely").unwrap();
        assert!(read_header(&path).is_err());
        let mut future = MAGIC.to_vec();
        future.extend(
            postcard::to_allocvec(&Header {
                format: FORMAT + 1,
                ..header
            })
            .unwrap(),
        );
        std::fs::write(&path, future).unwrap();
        assert!(read_header(&path).is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
