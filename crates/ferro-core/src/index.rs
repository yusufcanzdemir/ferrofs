//! In-memory index of one volume (SPEC §2.3).
//!
//! Struct-of-arrays keyed by a `u32` slot. Names live in one UTF-8 pool and their folded
//! (case-insensitive) copies in a second pool. Full paths are never stored; they are
//! rebuilt on demand by walking parents. Parents are stored as MFT record numbers rather
//! than slots, so records can arrive in any order and slots can be reused safely.

use crate::fold::{FoldOptions, fold_into};

/// Marker for "no slot" in `frn_to_slot`.
pub const NONE: u32 = u32::MAX;
/// MFT record number of the volume root directory.
pub const ROOT_RECORD: u32 = 5;
/// Deepest parent chain followed when rebuilding a path (guards against corrupt cycles).
const MAX_DEPTH: usize = 512;

/// Entry flags (`Index::flags`).
pub mod flags {
    pub const DIR: u16 = 1 << 0;
    pub const HIDDEN: u16 = 1 << 1;
    pub const SYSTEM: u16 = 1 << 2;
    pub const REPARSE: u16 = 1 << 3;
    pub const COMPRESSED: u16 = 1 << 4;
    pub const SPARSE: u16 = 1 << 5;
    /// Slot is on the free list.
    pub const FREE: u16 = 1 << 15;

    /// Converts Win32 `FILE_ATTRIBUTE_*` bits.
    pub const fn from_attributes(attributes: u32) -> u16 {
        let mut f = 0;
        if attributes & 0x10 != 0 {
            f |= DIR;
        }
        if attributes & 0x2 != 0 {
            f |= HIDDEN;
        }
        if attributes & 0x4 != 0 {
            f |= SYSTEM;
        }
        if attributes & 0x400 != 0 {
            f |= REPARSE;
        }
        if attributes & 0x800 != 0 {
            f |= COMPRESSED;
        }
        if attributes & 0x200 != 0 {
            f |= SPARSE;
        }
        f
    }
}

/// Lower 48 bits of a file reference number.
pub const fn record_number(frn: u64) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}

/// A file name as delivered by the source.
#[derive(Debug, Clone, Copy)]
pub enum Name<'a> {
    Utf8(&'a str),
    /// NTFS names are UTF-16 and may contain unpaired surrogates (replaced by U+FFFD).
    Utf16(&'a [u16]),
    /// UTF-16 little-endian bytes straight from an on-disk structure (no alignment needed).
    Utf16Le(&'a [u8]),
}

/// One file or folder to insert or update.
#[derive(Debug, Clone, Copy)]
pub struct NewEntry<'a> {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: Name<'a>,
    /// Win32 `FILE_ATTRIBUTE_*` bits.
    pub attributes: u32,
    pub size: u64,
    pub alloc_size: u64,
    /// FILETIME of the last modification.
    pub mtime: u64,
}

/// Index of one volume.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Index {
    /// e.g. `C:\`; prefix of every reconstructed path.
    volume_root: String,
    folded_root: Vec<u8>,
    fold_options: FoldOptions,

    frn_to_slot: Vec<u32>,
    frn: Vec<u64>,
    parent: Vec<u32>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    fold_off: Vec<u32>,
    fold_len: Vec<u16>,
    size: Vec<u64>,
    alloc_size: Vec<u64>,
    mtime: Vec<u64>,
    flags: Vec<u16>,

    name_pool: Vec<u8>,
    fold_pool: Vec<u8>,

    free: Vec<u32>,
    /// Pool bytes no longer referenced by any live slot.
    garbage_bytes: usize,
}

impl Index {
    pub fn new(volume_root: impl Into<String>, fold_options: FoldOptions) -> Self {
        Self::with_capacity(volume_root, fold_options, 0)
    }

    /// Pre-allocates for `entries` entries (avoids regrowth during a full scan).
    pub fn with_capacity(
        volume_root: impl Into<String>,
        fold_options: FoldOptions,
        entries: usize,
    ) -> Self {
        let volume_root = volume_root.into();
        Self {
            folded_root: crate::fold::fold(&volume_root, fold_options),
            volume_root,
            fold_options,
            frn_to_slot: Vec::with_capacity(entries + entries / 4),
            frn: Vec::with_capacity(entries),
            parent: Vec::with_capacity(entries),
            name_off: Vec::with_capacity(entries),
            name_len: Vec::with_capacity(entries),
            fold_off: Vec::with_capacity(entries),
            fold_len: Vec::with_capacity(entries),
            size: Vec::with_capacity(entries),
            alloc_size: Vec::with_capacity(entries),
            mtime: Vec::with_capacity(entries),
            flags: Vec::with_capacity(entries),
            name_pool: Vec::with_capacity(entries * 24),
            fold_pool: Vec::with_capacity(entries * 24),
            free: Vec::new(),
            garbage_bytes: 0,
        }
    }

    pub fn volume_root(&self) -> &str {
        &self.volume_root
    }

    pub fn fold_options(&self) -> FoldOptions {
        self.fold_options
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.flags.len() - self.free.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of slots (live + free); valid slots are `0..slot_count()`.
    pub fn slot_count(&self) -> usize {
        self.flags.len()
    }

    /// Inserts a new entry or updates the one with the same record number (rename, move,
    /// attribute change). Returns the slot, or `None` if the record number does not fit.
    pub fn upsert(&mut self, entry: &NewEntry<'_>) -> Option<u32> {
        let record = u32::try_from(record_number(entry.frn))
            .ok()
            .filter(|&r| r != NONE)?;
        let parent = u32::try_from(record_number(entry.parent_frn)).unwrap_or(NONE);

        let (name_off, name_len) = append_name(&mut self.name_pool, entry.name)?;
        let flags = flags::from_attributes(entry.attributes);
        let record_slot = record as usize;

        // Same name and parent (e.g. a USN "data changed" record): update metadata only,
        // so frequent writes do not grow the pools.
        if let Some(existing) = self
            .frn_to_slot
            .get(record_slot)
            .copied()
            .filter(|&s| s != NONE)
        {
            let s = existing as usize;
            let (old_pool, new_name) = self.name_pool.split_at(name_off as usize);
            let old_name = &old_pool[self.name_off[s] as usize..][..usize::from(self.name_len[s])];
            if self.parent[s] == parent && old_name == new_name {
                self.name_pool.truncate(name_off as usize);
                self.frn[s] = entry.frn;
                self.size[s] = entry.size;
                self.alloc_size[s] = entry.alloc_size;
                self.mtime[s] = entry.mtime;
                self.flags[s] = flags;
                return Some(existing);
            }
        }

        let name_bytes = &self.name_pool[name_off as usize..][..usize::from(name_len)];
        let name = std::str::from_utf8(name_bytes).unwrap_or_default();
        let fold_off = u32::try_from(self.fold_pool.len()).ok()?;
        fold_into(name, self.fold_options, &mut self.fold_pool);
        let fold_len = u16::try_from(self.fold_pool.len() - fold_off as usize).ok()?;

        if record_slot >= self.frn_to_slot.len() {
            self.frn_to_slot.resize(record_slot + 1, NONE);
        }
        let slot = match self.frn_to_slot[record_slot] {
            NONE => {
                let slot = self.alloc_slot();
                self.frn_to_slot[record_slot] = slot;
                slot
            }
            existing => {
                let s = existing as usize;
                self.garbage_bytes += usize::from(self.name_len[s]) + usize::from(self.fold_len[s]);
                existing
            }
        };

        let s = slot as usize;
        self.frn[s] = entry.frn;
        self.parent[s] = parent;
        self.name_off[s] = name_off;
        self.name_len[s] = name_len;
        self.fold_off[s] = fold_off;
        self.fold_len[s] = fold_len;
        self.size[s] = entry.size;
        self.alloc_size[s] = entry.alloc_size;
        self.mtime[s] = entry.mtime;
        self.flags[s] = flags;
        Some(slot)
    }

    fn alloc_slot(&mut self) -> u32 {
        if let Some(slot) = self.free.pop() {
            return slot;
        }
        let slot = u32::try_from(self.flags.len()).unwrap_or(NONE);
        self.frn.push(0);
        self.parent.push(NONE);
        self.name_off.push(0);
        self.name_len.push(0);
        self.fold_off.push(0);
        self.fold_len.push(0);
        self.size.push(0);
        self.alloc_size.push(0);
        self.mtime.push(0);
        self.flags.push(0);
        slot
    }

    /// Removes the entry with this file reference number. Returns false if unknown.
    pub fn remove(&mut self, frn: u64) -> bool {
        let Some(slot) = self.lookup(frn) else {
            return false;
        };
        let s = slot as usize;
        if let Ok(record) = usize::try_from(record_number(frn)) {
            self.frn_to_slot[record] = NONE;
        }
        self.garbage_bytes += usize::from(self.name_len[s]) + usize::from(self.fold_len[s]);
        self.name_len[s] = 0;
        self.fold_len[s] = 0;
        self.flags[s] = flags::FREE;
        self.parent[s] = NONE;
        self.free.push(slot);
        true
    }

    /// Updates size, allocation, modification time and attributes of an existing entry
    /// (e.g. after the file was written). Returns false if the record is unknown.
    pub fn set_metadata(
        &mut self,
        frn: u64,
        size: u64,
        alloc_size: u64,
        mtime: u64,
        attributes: u32,
    ) -> bool {
        let Some(slot) = self.lookup(frn) else {
            return false;
        };
        let s = slot as usize;
        self.size[s] = size;
        self.alloc_size[s] = alloc_size;
        self.mtime[s] = mtime;
        self.flags[s] = flags::from_attributes(attributes);
        true
    }

    /// Updates only the stream sizes of an existing entry.
    pub fn set_sizes(&mut self, frn: u64, size: u64, alloc_size: u64) -> bool {
        let Some(slot) = self.lookup(frn) else {
            return false;
        };
        self.size[slot as usize] = size;
        self.alloc_size[slot as usize] = alloc_size;
        true
    }

    /// Removes entries whose parent folder is not indexed (and, transitively, their
    /// children). After a full scan these are NTFS-internal items under `$Extend`, which
    /// would otherwise show up at the volume root. Returns how many were removed.
    pub fn remove_orphans(&mut self) -> usize {
        let mut removed = 0;
        loop {
            let orphans: Vec<u64> = (0..self.slot_count())
                .filter(|&s| self.flags[s] & flags::FREE == 0)
                .filter(|&s| {
                    let parent = self.parent[s];
                    parent != ROOT_RECORD
                        && self
                            .frn_to_slot
                            .get(parent as usize)
                            .is_none_or(|&p| p == NONE)
                })
                .map(|s| self.frn[s])
                .collect();
            if orphans.is_empty() {
                return removed;
            }
            for frn in orphans {
                if self.remove(frn) {
                    removed += 1;
                }
            }
        }
    }

    /// Slot of a file reference number (only the record number part is used).
    pub fn lookup(&self, frn: u64) -> Option<u32> {
        let record = usize::try_from(record_number(frn)).ok()?;
        self.frn_to_slot
            .get(record)
            .copied()
            .filter(|&slot| slot != NONE)
    }

    pub fn is_live(&self, slot: u32) -> bool {
        self.flags
            .get(slot as usize)
            .is_some_and(|f| f & flags::FREE == 0)
    }

    pub fn name(&self, slot: u32) -> &str {
        let s = slot as usize;
        let bytes = &self.name_pool[self.name_off[s] as usize..][..usize::from(self.name_len[s])];
        std::str::from_utf8(bytes).unwrap_or_default()
    }

    /// Case-folded name used for matching.
    pub fn folded_name(&self, slot: u32) -> &[u8] {
        let s = slot as usize;
        &self.fold_pool[self.fold_off[s] as usize..][..usize::from(self.fold_len[s])]
    }

    pub fn flags(&self, slot: u32) -> u16 {
        self.flags[slot as usize]
    }

    pub fn is_dir(&self, slot: u32) -> bool {
        self.flags(slot) & flags::DIR != 0
    }

    pub fn frn(&self, slot: u32) -> u64 {
        self.frn[slot as usize]
    }

    pub fn size(&self, slot: u32) -> u64 {
        self.size[slot as usize]
    }

    pub fn alloc_size(&self, slot: u32) -> u64 {
        self.alloc_size[slot as usize]
    }

    pub fn mtime(&self, slot: u32) -> u64 {
        self.mtime[slot as usize]
    }

    /// Slot of the parent folder, `None` at the volume root or if the parent is unknown.
    pub fn parent_of(&self, slot: u32) -> Option<u32> {
        let parent = self.parent[slot as usize];
        if parent == ROOT_RECORD || parent == NONE {
            return None;
        }
        self.frn_to_slot
            .get(parent as usize)
            .copied()
            .filter(|&p| p != NONE && p != slot)
    }

    /// True if the entry sits directly in the volume root folder.
    pub fn is_top_level(&self, slot: u32) -> bool {
        self.parent[slot as usize] == ROOT_RECORD
    }

    /// Slots from `slot` up to the top-level ancestor, written into `chain`.
    fn ancestors_into(&self, slot: u32, chain: &mut Vec<u32>) {
        chain.clear();
        let mut current = Some(slot);
        while let Some(s) = current {
            if chain.len() >= MAX_DEPTH {
                break;
            }
            chain.push(s);
            current = self.parent_of(s);
        }
    }

    /// Case-folded full path written into `out`, reusing both buffers (no allocation once
    /// they have grown). Used by `path:` filters.
    pub fn folded_path_into(&self, slot: u32, chain: &mut Vec<u32>, out: &mut Vec<u8>) {
        self.ancestors_into(slot, chain);
        out.clear();
        out.extend_from_slice(&self.folded_root);
        for (i, s) in chain.iter().rev().enumerate() {
            if i > 0 {
                out.push(b'\\');
            }
            out.extend_from_slice(self.folded_name(*s));
        }
    }

    /// Full path such as `C:\Users\me\file.txt`. Allocates; call only for result rows.
    pub fn path(&self, slot: u32) -> String {
        let mut chain = Vec::with_capacity(16);
        self.ancestors_into(slot, &mut chain);
        let mut path = self.volume_root.clone();
        for (i, s) in chain.iter().rev().enumerate() {
            if i > 0 {
                path.push('\\');
            }
            path.push_str(self.name(*s));
        }
        path
    }

    /// True when more than 20 % of slots are free or half the pool bytes are garbage.
    pub fn needs_compaction(&self) -> bool {
        let slots = self.slot_count();
        let pools = self.name_pool.len() + self.fold_pool.len();
        (slots > 0 && self.free.len() * 5 > slots) || (pools > 0 && self.garbage_bytes * 2 > pools)
    }

    /// Drops free slots and unreferenced pool bytes. Slot numbers change.
    pub fn compact(&mut self) {
        let live = self.len();
        let mut next = Self::with_capacity(self.volume_root.clone(), self.fold_options, live);
        next.frn_to_slot = vec![NONE; self.frn_to_slot.len()];
        for slot in 0..self.slot_count() {
            let s = slot as u32;
            if !self.is_live(s) {
                continue;
            }
            let new_slot = next.alloc_slot();
            let n = new_slot as usize;
            let name =
                &self.name_pool[self.name_off[slot] as usize..][..usize::from(self.name_len[slot])];
            let folded = self.folded_name(s);
            next.name_off[n] = u32::try_from(next.name_pool.len()).unwrap_or(0);
            next.name_pool.extend_from_slice(name);
            next.name_len[n] = self.name_len[slot];
            next.fold_off[n] = u32::try_from(next.fold_pool.len()).unwrap_or(0);
            next.fold_pool.extend_from_slice(folded);
            next.fold_len[n] = self.fold_len[slot];
            next.frn[n] = self.frn[slot];
            next.parent[n] = self.parent[slot];
            next.size[n] = self.size[slot];
            next.alloc_size[n] = self.alloc_size[slot];
            next.mtime[n] = self.mtime[slot];
            next.flags[n] = self.flags[slot];
            if let Ok(record) = usize::try_from(record_number(self.frn[slot])) {
                next.frn_to_slot[record] = new_slot;
            }
        }
        *self = next;
    }

    /// Releases spare capacity (e.g. after deserializing, where vectors grow by doubling).
    pub fn shrink_to_fit(&mut self) {
        self.frn_to_slot.shrink_to_fit();
        self.frn.shrink_to_fit();
        self.parent.shrink_to_fit();
        self.name_off.shrink_to_fit();
        self.name_len.shrink_to_fit();
        self.fold_off.shrink_to_fit();
        self.fold_len.shrink_to_fit();
        self.size.shrink_to_fit();
        self.alloc_size.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.flags.shrink_to_fit();
        self.name_pool.shrink_to_fit();
        self.fold_pool.shrink_to_fit();
        self.free.shrink_to_fit();
    }

    /// Heap bytes held by the index (capacity, not just length).
    pub fn heap_bytes(&self) -> usize {
        self.frn_to_slot.capacity() * 4
            + self.frn.capacity() * 8
            + self.parent.capacity() * 4
            + self.name_off.capacity() * 4
            + self.name_len.capacity() * 2
            + self.fold_off.capacity() * 4
            + self.fold_len.capacity() * 2
            + (self.size.capacity() + self.alloc_size.capacity() + self.mtime.capacity()) * 8
            + self.flags.capacity() * 2
            + self.name_pool.capacity()
            + self.fold_pool.capacity()
            + self.free.capacity() * 4
    }

    /// Heap bytes excluding the two name pools (the SPEC's per-entry budget).
    pub fn heap_bytes_without_names(&self) -> usize {
        self.heap_bytes() - self.name_pool.capacity() - self.fold_pool.capacity()
    }
}

fn push_utf16(pool: &mut Vec<u8>, units: impl Iterator<Item = u16>) {
    let mut buf = [0u8; 4];
    for c in char::decode_utf16(units) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        pool.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
}

/// Appends a name to the pool as UTF-8 without allocating; returns (offset, length).
fn append_name(pool: &mut Vec<u8>, name: Name<'_>) -> Option<(u32, u16)> {
    let off = pool.len();
    match name {
        Name::Utf8(s) => pool.extend_from_slice(s.as_bytes()),
        Name::Utf16(units) => push_utf16(pool, units.iter().copied()),
        Name::Utf16Le(bytes) => push_utf16(
            pool,
            bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]])),
        ),
    }
    let len = u16::try_from(pool.len() - off).ok();
    let off = u32::try_from(off).ok();
    match (off, len) {
        (Some(off), Some(len)) => Some((off, len)),
        _ => {
            pool.truncate(off.map_or(pool.len(), |o| o as usize));
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const DIR: u32 = 0x10;

    pub fn entry(frn: u64, parent: u64, name: &str, attributes: u32) -> NewEntry<'_> {
        NewEntry {
            frn,
            parent_frn: parent,
            name: Name::Utf8(name),
            attributes,
            size: 0,
            alloc_size: 0,
            mtime: 0,
        }
    }

    /// C:\Users\me\Docs\Report.PDF and friends; children before parents on purpose.
    pub fn sample() -> Index {
        let mut index = Index::new("C:\\", FoldOptions::default());
        let seq = 3u64 << 48; // sequence numbers must be ignored for lookups
        index.upsert(&entry(seq | 30, 22, "Report.PDF", 0)).unwrap();
        index.upsert(&entry(31, 22, "notes.txt", 0)).unwrap();
        index.upsert(&entry(22, 21, "Docs", DIR)).unwrap();
        index.upsert(&entry(21, 20, "me", DIR)).unwrap();
        index.upsert(&entry(20, 5, "Users", DIR)).unwrap();
        index.upsert(&entry(40, 5, "report-old.pdf", 0)).unwrap();
        index
    }

    #[test]
    fn paths_resolve_regardless_of_insert_order() {
        let index = sample();
        let report = index.lookup(30).unwrap();
        assert_eq!(index.path(report), r"C:\Users\me\Docs\Report.PDF");
        assert_eq!(index.folded_name(report), b"report.pdf");
        assert_eq!(index.path(index.lookup(40).unwrap()), r"C:\report-old.pdf");
        assert!(index.is_dir(index.lookup(22).unwrap()));
        assert_eq!(index.len(), 6);
    }

    #[test]
    fn rename_move_and_remove() {
        let mut index = sample();
        // Move notes.txt to the root and rename it.
        index.upsert(&entry(31, 5, "Notes-2025.txt", 0)).unwrap();
        let notes = index.lookup(31).unwrap();
        assert_eq!(index.path(notes), r"C:\Notes-2025.txt");
        assert_eq!(index.len(), 6);

        assert!(index.remove(30));
        assert!(!index.remove(30));
        assert!(index.lookup(30).is_none());
        assert_eq!(index.len(), 5);

        // The freed slot is reused.
        let slot = index.upsert(&entry(50, 22, "new.txt", 0)).unwrap();
        assert_eq!(index.path(slot), r"C:\Users\me\Docs\new.txt");
        assert_eq!(index.len(), 6);
    }

    #[test]
    fn unchanged_upsert_does_not_grow_pools() {
        let mut index = sample();
        let before = index.heap_bytes();
        let pool_len = index.name_pool.len();
        for _ in 0..1000 {
            index.upsert(&entry(31, 22, "notes.txt", 0x20)).unwrap();
        }
        assert_eq!(index.name_pool.len(), pool_len);
        assert_eq!(index.heap_bytes(), before);
        assert!(!index.needs_compaction());
    }

    #[test]
    fn utf16le_bytes_and_metadata_updates() {
        let mut index = Index::new("C:\\", FoldOptions::default());
        let bytes: Vec<u8> = "Ödev.pdf"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let slot = index
            .upsert(&NewEntry {
                name: Name::Utf16Le(&bytes[1..]), // deliberately unaligned slice start
                ..entry(9, 5, "", 0)
            })
            .unwrap();
        assert_ne!(index.name(slot), "Ödev.pdf"); // shifted by one byte: garbage, not a panic
        let slot = index
            .upsert(&NewEntry {
                name: Name::Utf16Le(&bytes),
                ..entry(9, 5, "", 0)
            })
            .unwrap();
        assert_eq!(index.name(slot), "Ödev.pdf");
        assert!(index.set_metadata(9, 1234, 4096, 77, 0x20));
        assert_eq!(
            (index.size(slot), index.alloc_size(slot), index.mtime(slot)),
            (1234, 4096, 77)
        );
        assert!(!index.set_metadata(999, 1, 1, 1, 0));
    }

    #[test]
    fn utf16_names_with_invalid_surrogates() {
        let mut index = Index::new("D:\\", FoldOptions::default());
        let name: Vec<u16> = "Şehir".encode_utf16().chain([0xD800]).collect();
        let slot = index
            .upsert(&NewEntry {
                name: Name::Utf16(&name),
                ..entry(7, 5, "", 0)
            })
            .unwrap();
        assert_eq!(index.name(slot), "Şehir\u{FFFD}");
        assert_eq!(index.folded_name(slot), "şehir\u{FFFD}".as_bytes());
    }

    #[test]
    fn compaction_keeps_entries_and_paths() {
        let mut index = sample();
        for frn in 100..200 {
            index.upsert(&entry(frn, 22, "tmp.bin", 0)).unwrap();
        }
        for frn in 100..200 {
            index.remove(frn);
        }
        assert!(index.needs_compaction());
        let before = index.heap_bytes();
        index.compact();
        assert!(!index.needs_compaction());
        assert!(index.heap_bytes() < before);
        assert_eq!(index.len(), 6);
        assert_eq!(index.slot_count(), 6);
        assert_eq!(
            index.path(index.lookup(30).unwrap()),
            r"C:\Users\me\Docs\Report.PDF"
        );
    }

    #[test]
    fn folded_path_matches_folded_display_path() {
        let index = sample();
        let (mut chain, mut out) = (Vec::new(), Vec::new());
        let slot = index.lookup(30).unwrap();
        index.folded_path_into(slot, &mut chain, &mut out);
        assert_eq!(
            out,
            crate::fold::fold(&index.path(slot), FoldOptions::default())
        );
    }

    #[test]
    fn orphans_are_removed_transitively() {
        let mut index = sample();
        index.upsert(&entry(70, 11, "$RmMetadata", DIR)).unwrap(); // parent $Extend: not indexed
        index.upsert(&entry(71, 70, "$Repair", DIR)).unwrap();
        index.upsert(&entry(72, 71, "deep.bin", 0)).unwrap();
        assert_eq!(index.remove_orphans(), 3);
        assert_eq!(index.len(), 6);
        assert!(index.lookup(72).is_none());
        assert_eq!(index.remove_orphans(), 0);
    }

    #[test]
    fn parent_cycles_do_not_hang() {
        let mut index = Index::new("C:\\", FoldOptions::default());
        index.upsert(&entry(10, 11, "a", DIR)).unwrap();
        index.upsert(&entry(11, 10, "b", DIR)).unwrap();
        let path = index.path(index.lookup(10).unwrap());
        assert!(path.len() < 4 * MAX_DEPTH);
    }
}
