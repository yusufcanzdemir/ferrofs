//! Raw `$MFT` reader (SPEC §2.2 phase B): names, hierarchy, sizes and timestamps.
//!
//! The MFT's own extents come from record 0. They are read sequentially in large chunks
//! on one thread while the previous chunk's records are parsed in parallel; the parsed
//! summaries are merged into the index on the calling thread.

use std::sync::mpsc;
use std::time::Instant;

use rayon::prelude::*;
use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};

use crate::error::{Error, Result};
use crate::fold::FoldOptions;
use crate::index::{Index, Name, NewEntry, record_number};
use crate::usn::{self, JournalInfo};
use crate::volume::Volume;

/// Bytes read per I/O request.
const CHUNK_BYTES: usize = 4 << 20;
/// Records 0..16 are NTFS metadata files ($MFT, $LogFile, …) and are not indexed.
const FIRST_USER_RECORD: u64 = 16;
const ROOT_RECORD: u64 = 5;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

const RECORD_IN_USE: u16 = 0x01;
const RECORD_IS_DIRECTORY: u16 = 0x02;
const NAMESPACE_DOS: u8 = 2;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Applies the update sequence array: checks the last two bytes of every sector against
/// the sequence number and restores the original bytes. False means a torn/corrupt record.
pub(crate) fn apply_fixups(record: &mut [u8], sector_size: usize) -> bool {
    if record.get(0..4) != Some(b"FILE") || sector_size < 2 {
        return false;
    }
    let (Some(usa_offset), Some(usa_count)) = (u16_at(record, 4), u16_at(record, 6)) else {
        return false;
    };
    let (usa_offset, usa_count) = (usize::from(usa_offset), usize::from(usa_count));
    if usa_count == 0 || (usa_count - 1) * sector_size > record.len() {
        return false;
    }
    let Some(check) = u16_at(record, usa_offset) else {
        return false;
    };
    for i in 1..usa_count {
        let end = i * sector_size;
        let Some(original) = u16_at(record, usa_offset + 2 * i) else {
            return false;
        };
        if u16_at(record, end - 2) != Some(check) {
            return false;
        }
        record[end - 2..end].copy_from_slice(&original.to_le_bytes());
    }
    true
}

/// Where the best file name of a record lives (byte offsets inside the record).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NameRef {
    pub parent: u64,
    pub offset: usize,
    pub bytes: usize,
    pub namespace: u8,
}

/// Summary of one FILE record. Plain data, no allocation, so parsing can run in parallel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub sequence: u16,
    pub is_dir: bool,
    /// Base record reference; non-zero for extension records.
    pub base: u64,
    /// `FILE_ATTRIBUTE_*` from `$STANDARD_INFORMATION`.
    pub attributes: u32,
    /// Last write time (FILETIME) from `$STANDARD_INFORMATION`.
    pub mtime: u64,
    pub name: Option<NameRef>,
    /// `(size, allocated)` of the unnamed `$DATA` stream.
    pub data: Option<(u64, u64)>,
}

/// One attribute inside a record.
struct Attribute<'a> {
    kind: u32,
    non_resident: bool,
    name_length: u8,
    bytes: &'a [u8],
    /// Offset of the attribute inside the record.
    offset: usize,
}

impl<'a> Attribute<'a> {
    /// Value of a resident attribute and its offset inside the record.
    fn resident_value(&self) -> Option<(&'a [u8], usize)> {
        if self.non_resident {
            return None;
        }
        let length = u32_at(self.bytes, 16)? as usize;
        let offset = usize::from(u16_at(self.bytes, 20)?);
        Some((
            self.bytes.get(offset..offset + length)?,
            self.offset + offset,
        ))
    }
}

fn attributes(record: &[u8]) -> impl Iterator<Item = Attribute<'_>> {
    let mut at = u16_at(record, 20).map_or(usize::MAX, usize::from);
    let in_use = u32_at(record, 24)
        .map_or(0, |n| n as usize)
        .min(record.len());
    std::iter::from_fn(move || {
        let kind = u32_at(record, at)?;
        let length = u32_at(record, at + 4)? as usize;
        if kind == ATTR_END || length < 16 || at + length > in_use {
            return None;
        }
        let attribute = Attribute {
            kind,
            non_resident: *record.get(at + 8)? != 0,
            name_length: *record.get(at + 9)?,
            bytes: &record[at..at + length],
            offset: at,
        };
        at += length;
        Some(attribute)
    })
}

/// Parses a fixed-up record. `None` for records that are not in use.
pub(crate) fn parse_record(record: &[u8]) -> Option<Parsed> {
    let flags = u16_at(record, 22)?;
    if flags & RECORD_IN_USE == 0 {
        return None;
    }
    let mut parsed = Parsed {
        sequence: u16_at(record, 16)?,
        is_dir: flags & RECORD_IS_DIRECTORY != 0,
        base: u64_at(record, 32)?,
        ..Parsed::default()
    };
    for attribute in attributes(record) {
        match attribute.kind {
            ATTR_STANDARD_INFORMATION => {
                if let Some((value, _)) = attribute.resident_value() {
                    parsed.mtime = u64_at(value, 8).unwrap_or(0);
                    parsed.attributes = u32_at(value, 32).unwrap_or(0);
                }
            }
            ATTR_FILE_NAME => {
                let Some((value, value_offset)) = attribute.resident_value() else {
                    continue;
                };
                let (Some(parent), Some(&units), Some(&namespace)) =
                    (u64_at(value, 0), value.get(64), value.get(65))
                else {
                    continue;
                };
                let bytes = usize::from(units) * 2;
                if namespace == NAMESPACE_DOS || value.len() < 66 + bytes {
                    continue;
                }
                let candidate = NameRef {
                    parent,
                    offset: value_offset + 66,
                    bytes,
                    namespace,
                };
                // Prefer a Win32 name (namespace 1 or 3) over a POSIX one (0).
                let better = match parsed.name {
                    None => true,
                    Some(current) => current.namespace == 0 && namespace != 0,
                };
                if better {
                    parsed.name = Some(candidate);
                }
            }
            // Only the unnamed stream; named ones are alternate data streams.
            ATTR_DATA if attribute.name_length == 0 => {
                if attribute.non_resident {
                    // Only the first piece (start VCN 0) carries the stream sizes.
                    if u64_at(attribute.bytes, 16) == Some(0) {
                        let allocated = u64_at(attribute.bytes, 40).unwrap_or(0);
                        let size = u64_at(attribute.bytes, 48).unwrap_or(0);
                        parsed.data = Some((size, allocated));
                    }
                } else if let Some(length) = u32_at(attribute.bytes, 16) {
                    // Resident data lives inside the MFT record and uses no clusters.
                    parsed.data = Some((u64::from(length), 0));
                }
            }
            _ => {}
        }
    }
    Some(parsed)
}

/// Decodes a run list into `(first cluster, cluster count)` pairs; sparse runs are skipped.
pub(crate) fn decode_runs(mut runs: &[u8]) -> Option<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    loop {
        let header = *runs.first()?;
        if header == 0 {
            return Some(out);
        }
        let (length_bytes, offset_bytes) = (usize::from(header & 0x0F), usize::from(header >> 4));
        if length_bytes == 0 || length_bytes > 8 || offset_bytes > 8 {
            return None;
        }
        let field = runs.get(1..1 + length_bytes + offset_bytes)?;
        let mut length = [0u8; 8];
        length[..length_bytes].copy_from_slice(&field[..length_bytes]);
        let length = u64::from_le_bytes(length);
        if offset_bytes > 0 {
            // Signed, little-endian, relative to the previous run.
            let delta_bytes = &field[length_bytes..];
            let fill = if delta_bytes[offset_bytes - 1] & 0x80 != 0 {
                0xFF
            } else {
                0
            };
            let mut delta = [fill; 8];
            delta[..offset_bytes].copy_from_slice(delta_bytes);
            lcn = lcn.checked_add(i64::from_le_bytes(delta))?;
            out.push((u64::try_from(lcn).ok()?, length));
        }
        runs = &runs[1 + length_bytes + offset_bytes..];
    }
}

/// Where the MFT is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftLayout {
    pub record_size: usize,
    pub sector_size: usize,
    pub cluster_size: u64,
    /// `(first cluster, cluster count)` in MFT order.
    pub extents: Vec<(u64, u64)>,
    /// Number of records covered by the valid data length.
    pub records: u64,
}

fn layout_error(reason: &str) -> Error {
    Error::Mft(reason.to_owned())
}

/// Reads the volume geometry and the extents of `$MFT` from its own record 0.
pub fn read_layout(volume: &Volume) -> Result<MftLayout> {
    let mut data = NTFS_VOLUME_DATA_BUFFER::default();
    usn::ioctl(volume, FSCTL_GET_NTFS_VOLUME_DATA, &(), &mut data)?;
    let record_size = data.BytesPerFileRecordSegment as usize;
    let sector_size = data.BytesPerSector as usize;
    let cluster_size = u64::from(data.BytesPerCluster);
    let valid =
        u64::try_from(data.MftValidDataLength).map_err(|_| layout_error("bad MFT length"))?;
    let start = u64::try_from(data.MftStartLcn).map_err(|_| layout_error("bad MFT start"))?;
    if record_size == 0 || sector_size == 0 || cluster_size == 0 || record_size % sector_size != 0 {
        return Err(layout_error("unexpected volume geometry"));
    }

    let mut record = vec![0u8; record_size.max(sector_size)];
    volume.read_at(start * cluster_size, &mut record)?;
    if !apply_fixups(&mut record[..record_size], sector_size) {
        return Err(layout_error("record 0 is damaged"));
    }
    let data_attribute = attributes(&record[..record_size])
        .find(|a| a.kind == ATTR_DATA && a.name_length == 0 && a.non_resident)
        .ok_or_else(|| layout_error("no $DATA in record 0"))?;
    let runs_offset = usize::from(u16_at(data_attribute.bytes, 32).unwrap_or(0));
    let extents = data_attribute
        .bytes
        .get(runs_offset..)
        .and_then(decode_runs)
        .ok_or_else(|| layout_error("bad $MFT run list"))?;
    let covered: u64 = extents.iter().map(|(_, n)| n * cluster_size).sum();
    if covered < valid {
        // The rest is described through $ATTRIBUTE_LIST (very fragmented MFT).
        return Err(layout_error("$MFT extents continue in an attribute list"));
    }
    Ok(MftLayout {
        record_size,
        sector_size,
        cluster_size,
        extents,
        records: valid / record_size as u64,
    })
}

/// A record that holds attributes of another (base) record.
struct Extension {
    base: u64,
    data: Option<(u64, u64)>,
    /// `(parent, namespace, byte range in the extension name pool)`.
    name: Option<(u64, u8, std::ops::Range<usize>)>,
}

/// A base record whose names all live in extension records (files with many hard links,
/// whose `$FILE_NAME` attributes do not fit the base record).
struct Nameless {
    frn: u64,
    attributes: u32,
    size: u64,
    alloc_size: u64,
    mtime: u64,
}

/// Builds the index of a volume from its MFT, including sizes and dates. Returns the
/// index and the journal position (taken before reading, so later changes are replayed).
pub fn scan(root: &str, fold_options: FoldOptions) -> Result<(Index, JournalInfo)> {
    let started = Instant::now();
    let volume = Volume::open(root)?;
    let journal = usn::query_journal(&volume)?;
    let layout = read_layout(&volume)?;
    let mut index = Index::with_capacity(
        root,
        fold_options,
        usize::try_from(layout.records).unwrap_or(0),
    );
    let record_size = layout.record_size;
    let chunk_bytes = CHUNK_BYTES / record_size * record_size;
    let total_bytes = layout.records * record_size as u64;
    let mut extensions = Vec::new();
    let mut extension_names: Vec<u8> = Vec::new();
    let mut nameless: std::collections::HashMap<u64, Nameless> = std::collections::HashMap::new();

    let (tx, rx) = mpsc::sync_channel::<(u64, Vec<u8>)>(3);
    let (volume_ref, layout_ref) = (&volume, &layout);
    std::thread::scope(|scope| -> Result<()> {
        // `tx` moves into the reader so the channel closes when reading is done;
        // otherwise the consumer loop below would wait forever.
        let reader = scope.spawn(move || -> Result<()> {
            let (volume, layout) = (volume_ref, layout_ref);
            let mut first_record = 0u64;
            let mut remaining = total_bytes;
            for &(lcn, clusters) in &layout.extents {
                let mut offset = lcn * layout.cluster_size;
                let mut extent_left = (clusters * layout.cluster_size).min(remaining);
                while extent_left > 0 {
                    let len =
                        (chunk_bytes as u64).min(extent_left) as usize / record_size * record_size;
                    if len == 0 {
                        break;
                    }
                    let mut buffer = vec![0u8; len];
                    volume.read_at(offset, &mut buffer)?;
                    if tx.send((first_record, buffer)).is_err() {
                        return Ok(()); // the consumer stopped
                    }
                    first_record += (len / record_size) as u64;
                    offset += len as u64;
                    extent_left -= len as u64;
                    remaining -= len as u64;
                }
            }
            Ok(())
        });

        for (first_record, mut buffer) in rx {
            let parsed: Vec<Option<Parsed>> = buffer
                .par_chunks_mut(record_size)
                .map(|record| {
                    apply_fixups(record, layout.sector_size)
                        .then(|| parse_record(record))
                        .flatten()
                })
                .collect();
            for (i, parsed) in parsed.iter().enumerate() {
                let Some(parsed) = parsed else { continue };
                let record_no = first_record + i as u64;
                let record = &buffer[i * record_size..(i + 1) * record_size];
                if parsed.base != 0 {
                    let name = parsed.name.map(|n| {
                        let start = extension_names.len();
                        extension_names.extend_from_slice(&record[n.offset..n.offset + n.bytes]);
                        (n.parent, n.namespace, start..extension_names.len())
                    });
                    if parsed.data.is_some() || name.is_some() {
                        extensions.push(Extension {
                            base: record_number(parsed.base),
                            data: parsed.data,
                            name,
                        });
                    }
                    continue;
                }
                if record_no < FIRST_USER_RECORD {
                    continue; // NTFS metadata files
                }
                let mut attributes = parsed.attributes;
                if parsed.is_dir {
                    attributes |= FILE_ATTRIBUTE_DIRECTORY;
                }
                let (size, alloc_size) = if parsed.is_dir {
                    (0, 0)
                } else {
                    parsed.data.unwrap_or((0, 0))
                };
                let frn = record_no | (u64::from(parsed.sequence) << 48);
                let Some(name) = parsed.name else {
                    nameless.insert(
                        record_no,
                        Nameless {
                            frn,
                            attributes,
                            size,
                            alloc_size,
                            mtime: parsed.mtime,
                        },
                    );
                    continue;
                };
                if !is_indexed_parent(name.parent) {
                    continue; // children of metadata folders ($Extend\…)
                }
                index.upsert(&NewEntry {
                    frn,
                    parent_frn: name.parent,
                    name: Name::Utf16Le(&record[name.offset..name.offset + name.bytes]),
                    attributes,
                    size,
                    alloc_size,
                    mtime: parsed.mtime,
                });
            }
        }
        reader.join().unwrap_or(Ok(()))
    })?;

    // Attributes that live in extension records: stream sizes of heavily fragmented files
    // and names of files with many hard links.
    let mut best_names: std::collections::HashMap<u64, (u64, u8, std::ops::Range<usize>)> =
        std::collections::HashMap::new();
    for extension in &extensions {
        if let Some((size, alloc)) = extension.data {
            match nameless.get_mut(&extension.base) {
                Some(pending) => (pending.size, pending.alloc_size) = (size, alloc),
                None => {
                    index.set_sizes(extension.base, size, alloc);
                }
            }
        }
        if let Some((parent, namespace, range)) = &extension.name
            && nameless.contains_key(&extension.base)
        {
            let better = best_names
                .get(&extension.base)
                .is_none_or(|(_, current, _)| *current == 0 && *namespace != 0);
            if better {
                best_names.insert(extension.base, (*parent, *namespace, range.clone()));
            }
        }
    }
    for (record_no, pending) in &nameless {
        let Some((parent, _, range)) = best_names.get(record_no) else {
            continue;
        };
        if !is_indexed_parent(*parent) {
            continue;
        }
        index.upsert(&NewEntry {
            frn: pending.frn,
            parent_frn: *parent,
            name: Name::Utf16Le(&extension_names[range.clone()]),
            attributes: pending.attributes,
            size: pending.size,
            alloc_size: pending.alloc_size,
            mtime: pending.mtime,
        });
    }
    let orphans = index.remove_orphans();
    tracing::info!(
        root,
        orphans,
        records = layout.records,
        entries = index.len(),
        ms = started.elapsed().as_millis(),
        "MFT scanned"
    );
    Ok((index, journal))
}

/// Entries under metadata folders such as `$Extend` are not indexed.
fn is_indexed_parent(parent_frn: u64) -> bool {
    let parent = record_number(parent_frn);
    parent >= FIRST_USER_RECORD || parent == ROOT_RECORD
}

/// Scans `root` from the MFT (with sizes and dates), falling back to USN enumeration
/// (names only) when the MFT cannot be read.
pub fn scan_volume(root: &str, fold_options: FoldOptions) -> Result<(Index, JournalInfo)> {
    match scan(root, fold_options) {
        Ok(result) => Ok(result),
        Err(err @ Error::Mft(_)) => {
            tracing::warn!(root, "MFT scan failed ({err}); using USN enumeration");
            usn::scan(root, fold_options)
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a resident attribute.
    pub fn resident(kind: u32, value: &[u8]) -> Vec<u8> {
        let length = (24 + value.len()).next_multiple_of(8);
        let mut a = vec![0u8; length];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[4..8].copy_from_slice(&u32::try_from(length).unwrap().to_le_bytes());
        a[16..20].copy_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a[24..24 + value.len()].copy_from_slice(value);
        a
    }

    /// Builds a non-resident unnamed $DATA attribute.
    pub fn non_resident_data(start_vcn: u64, allocated: u64, size: u64, runs: &[u8]) -> Vec<u8> {
        let length = (64 + runs.len() + 1).next_multiple_of(8);
        let mut a = vec![0u8; length];
        a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        a[4..8].copy_from_slice(&u32::try_from(length).unwrap().to_le_bytes());
        a[8] = 1;
        a[16..24].copy_from_slice(&start_vcn.to_le_bytes());
        a[32..34].copy_from_slice(&64u16.to_le_bytes());
        a[40..48].copy_from_slice(&allocated.to_le_bytes());
        a[48..56].copy_from_slice(&size.to_le_bytes());
        a[56..64].copy_from_slice(&size.to_le_bytes());
        a[64..64 + runs.len()].copy_from_slice(runs);
        a
    }

    pub fn standard_information(mtime: u64, attributes: u32) -> Vec<u8> {
        let mut v = vec![0u8; 48];
        v[8..16].copy_from_slice(&mtime.to_le_bytes());
        v[32..36].copy_from_slice(&attributes.to_le_bytes());
        resident(ATTR_STANDARD_INFORMATION, &v)
    }

    pub fn file_name(parent: u64, namespace: u8, name: &str) -> Vec<u8> {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut v = vec![0u8; 66 + units.len()];
        v[0..8].copy_from_slice(&parent.to_le_bytes());
        v[64] = u8::try_from(units.len() / 2).unwrap();
        v[65] = namespace;
        v[66..].copy_from_slice(&units);
        resident(ATTR_FILE_NAME, &v)
    }

    /// A 1 KiB FILE record with the update sequence applied as NTFS writes it.
    pub fn record(sequence: u16, flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&48u16.to_le_bytes()); // update sequence array offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 + two 512-byte sectors
        r[16..18].copy_from_slice(&sequence.to_le_bytes());
        r[20..22].copy_from_slice(&56u16.to_le_bytes()); // first attribute
        r[22..24].copy_from_slice(&flags.to_le_bytes());
        r[32..40].copy_from_slice(&base.to_le_bytes());
        let mut at = 56;
        for a in attrs {
            r[at..at + a.len()].copy_from_slice(a);
            at += a.len();
        }
        r[at..at + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        r[24..28].copy_from_slice(&u32::try_from(at + 8).unwrap().to_le_bytes());
        r[28..32].copy_from_slice(&1024u32.to_le_bytes());
        // Update sequence: number 0x0102, originals saved, sector tails overwritten.
        r[48..50].copy_from_slice(&0x0102u16.to_le_bytes());
        for i in 1..3 {
            let end = i * 512;
            let original = [r[end - 2], r[end - 1]];
            r[48 + 2 * i..50 + 2 * i].copy_from_slice(&original);
            r[end - 2..end].copy_from_slice(&0x0102u16.to_le_bytes());
        }
        r
    }

    #[test]
    fn parses_a_file_record() {
        let mut r = record(
            7,
            RECORD_IN_USE,
            0,
            &[
                standard_information(133_617_168_000_000_000, 0x22),
                file_name(40 | (2 << 48), NAMESPACE_DOS, "REPORT~1.PDF"),
                file_name(40 | (2 << 48), 1, "Rapor Şubat.pdf"),
                non_resident_data(0, 8192, 5000, &[0x11, 0x02, 0x10, 0x00]),
            ],
        );
        assert!(apply_fixups(&mut r, 512));
        let parsed = parse_record(&r).unwrap();
        assert_eq!(parsed.sequence, 7);
        assert!(!parsed.is_dir);
        assert_eq!(parsed.base, 0);
        assert_eq!(parsed.attributes, 0x22);
        assert_eq!(parsed.mtime, 133_617_168_000_000_000);
        assert_eq!(parsed.data, Some((5000, 8192)));
        let name = parsed.name.unwrap();
        assert_eq!(name.parent, 40 | (2 << 48));
        assert_eq!(name.namespace, 1, "the DOS 8.3 name must be skipped");
        let units: Vec<u16> = r[name.offset..name.offset + name.bytes]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "Rapor Şubat.pdf");
    }

    #[test]
    fn resident_data_directories_and_free_records() {
        let mut small = record(
            1,
            RECORD_IN_USE,
            0,
            &[file_name(5, 3, "a.txt"), resident(ATTR_DATA, b"hello")],
        );
        assert!(apply_fixups(&mut small, 512));
        assert_eq!(parse_record(&small).unwrap().data, Some((5, 0)));

        let mut dir = record(
            1,
            RECORD_IN_USE | RECORD_IS_DIRECTORY,
            0,
            &[file_name(5, 3, "docs")],
        );
        assert!(apply_fixups(&mut dir, 512));
        assert!(parse_record(&dir).unwrap().is_dir);

        let mut free = record(1, 0, 0, &[file_name(5, 3, "gone.txt")]);
        assert!(apply_fixups(&mut free, 512));
        assert!(parse_record(&free).is_none());
    }

    #[test]
    fn torn_records_are_rejected() {
        let mut r = record(1, RECORD_IN_USE, 0, &[file_name(5, 3, "x")]);
        r[1022] ^= 0xFF; // the sector tail no longer matches the update sequence number
        assert!(!apply_fixups(&mut r, 512));
        let mut not_file = vec![0u8; 1024];
        assert!(!apply_fixups(&mut not_file, 512));
    }

    #[test]
    fn named_data_streams_are_ignored() {
        let mut ads = non_resident_data(0, 4096, 99, &[0x11, 0x01, 0x05, 0x00]);
        ads[9] = 4; // name length: an alternate data stream
        let mut r = record(1, RECORD_IN_USE, 0, &[file_name(5, 3, "x"), ads]);
        assert!(apply_fixups(&mut r, 512));
        assert_eq!(parse_record(&r).unwrap().data, None);
    }

    #[test]
    fn run_lists() {
        // 0x21: 1-byte length, 2-byte offset. Runs: 0x18 clusters at 0x5634, then
        // 0x10 clusters at -0x100 relative, then a sparse run, then the end marker.
        let runs = [
            0x21, 0x18, 0x34, 0x56, 0x21, 0x10, 0x00, 0xFF, 0x01, 0x08, 0x00,
        ];
        assert_eq!(
            decode_runs(&runs),
            Some(vec![(0x5634, 0x18), (0x5534, 0x10)])
        );
        assert_eq!(decode_runs(&[0x00]), Some(vec![]));
        assert_eq!(decode_runs(&[0x21, 0x18]), None, "truncated run");
    }
}
