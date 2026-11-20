//! Folder size totals for the storage analyzer (SPEC §5.1).
//!
//! [`SizeTotals::build`] aggregates file sizes bottom-up over one volume index. The index
//! is large and its slots are in MFT order, not tree order, so the build avoids walking
//! the tree entry by entry (random memory access dominated an earlier version):
//!
//! 1. One sequential pass numbers the folders and finds each entry's parent folder.
//! 2. Folder depths are computed with memoized parent walks, then folders are bucketed by
//!    depth.
//! 3. Files are added to their parent folder in one sequential pass.
//! 4. Folders are added to their parents deepest first, so each is complete before it is
//!    added.
//! 5. Each folder gets a pre-order interval over the folder tree, so "is this file below
//!    folder X" is two comparisons. Largest-file and extension reports are then parallel
//!    sequential scans.
//!
//! Hard links are counted once because the index holds one slot per MFT record. Entries
//! whose parent is unknown (or that sit in a parent cycle) are not reachable from the
//! volume root and are left out.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};

use rayon::prelude::*;

use crate::fold::fold;
use crate::index::Index;

/// `dir_id` of a file.
const NOT_DIR: u32 = u32::MAX;
/// `parent_dir` of an entry in the volume root folder.
const TOP: u32 = u32::MAX - 1;
/// `parent_dir` / `depth` / `tour_in` of an entry that is not reachable from the root.
const LOST: u32 = u32::MAX;
/// Slots per parallel work item in the report scans.
const CHUNK: usize = 1 << 15;

/// Totals of one file, folder or volume.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Logical bytes (end of file).
    pub size: u64,
    /// Bytes allocated on disk (smaller than `size` for compressed or sparse files).
    pub alloc_size: u64,
    pub files: u64,
    /// Sub-folders, recursively.
    pub folders: u64,
    /// Newest modification time (FILETIME) in the subtree.
    pub mtime: u64,
}

/// Bytes per extension (see [`SizeTotals::extensions`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionTotal {
    /// Folded extension without the dot; empty for files without one.
    pub ext: String,
    pub size: u64,
    pub alloc_size: u64,
    pub files: u64,
}

/// Folder totals and child lists of one volume index. Valid only for the index state it
/// was built from (slots change on compaction); every method takes that same index.
#[derive(Debug, Clone)]
pub struct SizeTotals {
    /// Files that count (`None` = all).
    include: Option<Vec<bool>>,
    /// Per slot: folder number, or `NOT_DIR`.
    dir_id: Vec<u32>,
    /// Per slot: folder number of the parent, `TOP` or `LOST`.
    parent_dir: Vec<u32>,

    // Per folder number.
    dir_slot: Vec<u32>,
    size: Vec<u64>,
    alloc_size: Vec<u64>,
    mtime: Vec<u64>,
    files: Vec<u32>,
    folders: Vec<u32>,
    /// Pre-order position in the folder tree (`LOST` if unreachable); the folders below
    /// folder `d` are exactly those with `tour_in` in `tour_in[d] + 1..tour_end[d]`.
    tour_in: Vec<u32>,
    tour_end: Vec<u32>,
    /// Children of folder `d` (unsorted, hidden ones included) are
    /// `children[child_start[d]..child_start[d + 1]]`.
    child_start: Vec<u32>,
    children: Vec<u32>,

    /// Entries in the volume root folder.
    top: Vec<u32>,
    root: Totals,
}

impl SizeTotals {
    /// Aggregates `index`. With `include` (one flag per slot, see
    /// [`crate::query::match_mask`]) only those files count, and folders without any of
    /// them are hidden; without it every file counts and every folder is shown.
    pub fn build(index: &Index, include: Option<Vec<bool>>) -> Self {
        let n = index.slot_count();
        let filtered = include.is_some();

        // 1. Folder numbers and parent folders.
        let mut dir_id = vec![NOT_DIR; n];
        let mut dir_slot = Vec::new();
        for slot in slots(n) {
            if index.is_live(slot) && index.is_dir(slot) {
                dir_id[slot as usize] = to_u32(dir_slot.len());
                dir_slot.push(slot);
            }
        }
        let dirs = dir_slot.len();
        let mut parent_dir = vec![LOST; n];
        parent_dir
            .par_chunks_mut(CHUNK)
            .enumerate()
            .for_each(|(chunk, out)| {
                for (i, parent) in out.iter_mut().enumerate() {
                    let slot = to_u32(chunk * CHUNK + i);
                    if !index.is_live(slot) {
                        continue;
                    }
                    *parent = if index.is_top_level(slot) {
                        TOP
                    } else {
                        match index.parent_of(slot) {
                            Some(p) if dir_id[p as usize] != NOT_DIR => dir_id[p as usize],
                            _ => LOST,
                        }
                    };
                }
            });

        // 2. Depths, then folders in order of increasing depth.
        let depth = folder_depths(&dir_slot, &parent_dir);
        let order = by_depth(&depth);

        // 3. Files into their parent folders.
        let mut size = vec![0u64; dirs];
        let mut alloc_size = vec![0u64; dirs];
        let mut mtime = vec![0u64; dirs];
        let mut files = vec![0u32; dirs];
        let mut folders = vec![0u32; dirs];
        if !filtered {
            for (d, &slot) in dir_slot.iter().enumerate() {
                mtime[d] = index.mtime(slot);
            }
        }
        let mut root = Totals::default();
        for slot in slots(n) {
            let s = slot as usize;
            if dir_id[s] != NOT_DIR
                || !index.is_live(slot)
                || include.as_ref().is_some_and(|mask| !mask[s])
            {
                continue;
            }
            let (file_size, file_alloc, file_mtime) =
                (index.size(slot), index.alloc_size(slot), index.mtime(slot));
            match parent_dir[s] {
                TOP => {
                    root.size += file_size;
                    root.alloc_size += file_alloc;
                    root.files += 1;
                    root.mtime = root.mtime.max(file_mtime);
                }
                LOST => {}
                p if depth[p as usize] == LOST => {}
                p => {
                    let p = p as usize;
                    size[p] += file_size;
                    alloc_size[p] += file_alloc;
                    files[p] = files[p].saturating_add(1);
                    mtime[p] = mtime[p].max(file_mtime);
                }
            }
        }

        // 4. Folders into their parents, deepest first. Also counts the folders below each
        //    folder (hidden ones included) for the pre-order intervals.
        let mut below = vec![0u32; dirs];
        for &d in order.iter().rev() {
            let d = d as usize;
            let visible = !filtered || files[d] > 0;
            match parent_dir[dir_slot[d] as usize] {
                TOP => {
                    if visible {
                        root.size += size[d];
                        root.alloc_size += alloc_size[d];
                        root.files += u64::from(files[d]);
                        root.folders += u64::from(folders[d]) + 1;
                        root.mtime = root.mtime.max(mtime[d]);
                    }
                }
                p => {
                    let p = p as usize;
                    below[p] += below[d] + 1;
                    if visible {
                        size[p] += size[d];
                        alloc_size[p] += alloc_size[d];
                        files[p] = files[p].saturating_add(files[d]);
                        folders[p] = folders[p].saturating_add(folders[d] + 1);
                        mtime[p] = mtime[p].max(mtime[d]);
                    }
                }
            }
        }

        // 5. Pre-order intervals, shallowest first: each folder hands out consecutive
        //    ranges to its sub-folders.
        let mut tour_in = vec![LOST; dirs];
        let mut tour_end = vec![LOST; dirs];
        let mut next = vec![0u32; dirs];
        let mut next_top = 0u32;
        for &d in &order {
            let d = d as usize;
            let cursor = match parent_dir[dir_slot[d] as usize] {
                TOP => &mut next_top,
                p => &mut next[p as usize],
            };
            let start = *cursor;
            *cursor += below[d] + 1;
            tour_in[d] = start;
            tour_end[d] = start + below[d] + 1;
            next[d] = start + 1;
        }
        drop((below, next, order));

        // Child lists of reachable folders, and the root's entries.
        let mut child_start = vec![0u32; dirs + 1];
        let mut top = Vec::new();
        for slot in slots(n) {
            match parent_dir[slot as usize] {
                TOP => top.push(slot),
                LOST => {}
                p if depth[p as usize] == LOST => {}
                p => child_start[p as usize + 1] += 1,
            }
        }
        for d in 1..=dirs {
            child_start[d] += child_start[d - 1];
        }
        let mut cursor = child_start.clone();
        let mut children = vec![0u32; child_start[dirs] as usize];
        for slot in slots(n) {
            match parent_dir[slot as usize] {
                TOP | LOST => {}
                p if depth[p as usize] == LOST => {}
                p => {
                    let at = &mut cursor[p as usize];
                    children[*at as usize] = slot;
                    *at += 1;
                }
            }
        }

        Self {
            include,
            dir_id,
            parent_dir,
            dir_slot,
            size,
            alloc_size,
            mtime,
            files,
            folders,
            tour_in,
            tour_end,
            child_start,
            children,
            top,
            root,
        }
    }

    fn included(&self, slot: u32) -> bool {
        self.include
            .as_ref()
            .is_none_or(|mask| mask.get(slot as usize) == Some(&true))
    }

    /// True if the entry is shown: matching files, and folders containing any (all
    /// folders without a filter).
    fn visible(&self, index: &Index, slot: u32) -> bool {
        match self.dir_id.get(slot as usize) {
            None => false,
            Some(&NOT_DIR) => index.is_live(slot) && self.included(slot),
            Some(&d) => self.include.is_none() || self.files[d as usize] > 0,
        }
    }

    fn raw_children(&self, folder: Option<u32>) -> &[u32] {
        let Some(slot) = folder else {
            return &self.top;
        };
        match self.dir_id.get(slot as usize) {
            Some(&d) if d != NOT_DIR => {
                let d = d as usize;
                &self.children[self.child_start[d] as usize..self.child_start[d + 1] as usize]
            }
            _ => &[],
        }
    }

    /// True if `folder` (`None` = volume root) has a child, visible or not, whose folded
    /// name is one of `names`.
    pub fn has_child_named(&self, index: &Index, folder: Option<u32>, names: &[&[u8]]) -> bool {
        self.raw_children(folder)
            .iter()
            .any(|&c| index.is_live(c) && names.contains(&index.folded_name(c)))
    }

    /// Predicate "this slot is a folder reachable from the root and strictly below
    /// `folder`" (`None` = anywhere on the volume).
    pub fn folder_below(&self, folder: Option<u32>) -> impl Fn(u32) -> bool + Sync {
        let range = match folder {
            None => Some((0, u32::MAX)),
            Some(slot) => match self.dir_id.get(slot as usize) {
                Some(&d) if d != NOT_DIR && self.tour_in[d as usize] != LOST => {
                    Some((self.tour_in[d as usize] + 1, self.tour_end[d as usize]))
                }
                _ => None,
            },
        };
        move |slot: u32| {
            let Some((low, high)) = range else {
                return false;
            };
            match self.dir_id.get(slot as usize) {
                Some(&d) if d != NOT_DIR => {
                    let position = self.tour_in[d as usize];
                    position != LOST && position >= low && position < high
                }
                _ => false,
            }
        }
    }

    /// Visible children of `folder` (`None` = volume root), largest first (ties by slot).
    pub fn children(&self, index: &Index, folder: Option<u32>) -> Vec<u32> {
        let mut list: Vec<(u64, u32)> = self
            .raw_children(folder)
            .iter()
            .filter(|&&c| self.visible(index, c))
            .map(|&c| (self.totals(index, c).size, c))
            .collect();
        list.sort_unstable_by_key(|&(size, slot)| (Reverse(size), slot));
        list.into_iter().map(|(_, slot)| slot).collect()
    }

    /// Number of visible children (no sorting).
    pub fn child_count(&self, index: &Index, folder: Option<u32>) -> usize {
        self.raw_children(folder)
            .iter()
            .filter(|&&c| self.visible(index, c))
            .count()
    }

    /// Totals of one entry: a file's own values (zero if the filter excludes it), or a
    /// folder's subtree.
    pub fn totals(&self, index: &Index, slot: u32) -> Totals {
        match self.dir_id.get(slot as usize) {
            None => Totals::default(),
            Some(&NOT_DIR) if self.included(slot) => Totals {
                size: index.size(slot),
                alloc_size: index.alloc_size(slot),
                files: 1,
                folders: 0,
                mtime: index.mtime(slot),
            },
            Some(&NOT_DIR) => Totals::default(),
            Some(&d) => {
                let d = d as usize;
                Totals {
                    size: self.size[d],
                    alloc_size: self.alloc_size[d],
                    files: u64::from(self.files[d]),
                    folders: u64::from(self.folders[d]),
                    mtime: self.mtime[d],
                }
            }
        }
    }

    /// Totals of the whole volume.
    pub fn root(&self) -> Totals {
        self.root
    }

    /// Totals of `folder` (`None` = volume root).
    pub fn totals_of(&self, index: &Index, folder: Option<u32>) -> Totals {
        folder.map_or(self.root, |slot| self.totals(index, slot))
    }

    /// Finds the visible entry at `relative` (components separated by `\`, relative to the
    /// volume root; empty = the root itself, returned as `Some(None)`). Matching is
    /// case-insensitive with the index's folding rules.
    pub fn resolve(&self, index: &Index, relative: &str) -> Option<Option<u32>> {
        let mut current = None;
        for component in relative.split(['\\', '/']).filter(|c| !c.is_empty()) {
            let folded = fold(component, index.fold_options());
            let found =
                self.raw_children(current).iter().copied().find(|&c| {
                    index.folded_name(c) == folded.as_slice() && self.visible(index, c)
                })?;
            current = Some(found);
        }
        Some(current)
    }

    /// Predicate "this live file counts (matches the filter) and lies below `folder`"
    /// (`None` = anywhere on the volume).
    pub fn file_below(&self, index: &Index, folder: Option<u32>) -> impl Fn(u32) -> bool + Sync {
        // (lowest, highest) pre-order positions of the parent folder; `None` = only TOP.
        let range = match folder {
            None => Some((0, u32::MAX)),
            Some(slot) => match self.dir_id.get(slot as usize) {
                Some(&d) if d != NOT_DIR && self.tour_in[d as usize] != LOST => {
                    Some((self.tour_in[d as usize], self.tour_end[d as usize]))
                }
                _ => None,
            },
        };
        let all = folder.is_none();
        move |slot: u32| {
            let s = slot as usize;
            if self.dir_id[s] != NOT_DIR || !index.is_live(slot) || !self.included(slot) {
                return false;
            }
            match (self.parent_dir[s], range) {
                (TOP, _) => all,
                (LOST, _) | (_, None) => false,
                (p, Some((low, high))) => {
                    let position = self.tour_in[p as usize];
                    position != LOST && position >= low && position < high
                }
            }
        }
    }

    /// The `n` largest files below `folder`, largest first (ties by slot).
    pub fn largest_files(&self, index: &Index, folder: Option<u32>, n: usize) -> Vec<u32> {
        if n == 0 {
            return Vec::new();
        }
        let below = self.file_below(index, folder);
        let mut best: Vec<(u64, Reverse<u32>)> = (0..self.dir_id.len().div_ceil(CHUNK))
            .into_par_iter()
            .map(|chunk| {
                let mut heap = BinaryHeap::with_capacity(n + 1);
                let end = ((chunk + 1) * CHUNK).min(self.dir_id.len());
                for slot in slots_in(chunk * CHUNK, end) {
                    if below(slot) {
                        heap.push(Reverse((index.size(slot), Reverse(slot))));
                        if heap.len() > n {
                            heap.pop();
                        }
                    }
                }
                heap.into_iter()
                    .map(|Reverse(item)| item)
                    .collect::<Vec<_>>()
            })
            .flatten()
            .collect();
        best.sort_unstable_by(|a, b| b.cmp(a));
        best.truncate(n);
        best.into_iter().map(|(_, Reverse(slot))| slot).collect()
    }

    /// The `n` extensions with the most bytes below `folder`.
    pub fn extensions(&self, index: &Index, folder: Option<u32>, n: usize) -> Vec<ExtensionTotal> {
        if n == 0 {
            return Vec::new();
        }
        let below = self.file_below(index, folder);
        let merged = (0..self.dir_id.len().div_ceil(CHUNK))
            .into_par_iter()
            .map(|chunk| {
                let mut by_ext = ExtMap::default();
                let end = ((chunk + 1) * CHUNK).min(self.dir_id.len());
                for slot in slots_in(chunk * CHUNK, end) {
                    if below(slot) {
                        let entry = by_ext
                            .entry(extension(index.folded_name(slot)))
                            .or_default();
                        entry.0 += index.size(slot);
                        entry.1 += index.alloc_size(slot);
                        entry.2 += 1;
                    }
                }
                by_ext
            })
            .reduce(ExtMap::default, |mut a, b| {
                for (ext, (size, alloc, count)) in b {
                    let entry = a.entry(ext).or_default();
                    entry.0 += size;
                    entry.1 += alloc;
                    entry.2 += count;
                }
                a
            });
        let mut list: Vec<_> = merged.into_iter().collect();
        list.sort_unstable_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
        list.truncate(n);
        list.into_iter()
            .map(|(ext, (size, alloc_size, files))| ExtensionTotal {
                ext: String::from_utf8_lossy(ext).into_owned(),
                size,
                alloc_size,
                files,
            })
            .collect()
    }

    /// Heap bytes held (for logging).
    pub fn heap_bytes(&self) -> usize {
        self.include.as_ref().map_or(0, Vec::capacity)
            + (self.size.capacity() + self.alloc_size.capacity() + self.mtime.capacity()) * 8
            + (self.dir_id.capacity()
                + self.parent_dir.capacity()
                + self.dir_slot.capacity()
                + self.files.capacity()
                + self.folders.capacity()
                + self.tour_in.capacity()
                + self.tour_end.capacity()
                + self.child_start.capacity()
                + self.children.capacity()
                + self.top.capacity())
                * 4
    }
}

/// Depth of each folder below the root (0 = in the root folder), `LOST` if its parent
/// chain does not reach the root.
fn folder_depths(dir_slot: &[u32], parent_dir: &[u32]) -> Vec<u32> {
    const UNSET: u32 = u32::MAX - 1;
    const VISITING: u32 = u32::MAX - 2;
    let mut depth = vec![UNSET; dir_slot.len()];
    let mut chain = Vec::new();
    for start in 0..dir_slot.len() {
        if depth[start] != UNSET {
            continue;
        }
        chain.clear();
        let mut current = start;
        // Walk up until a folder with a known depth, the root, or a dead end.
        let base = loop {
            chain.push(current);
            depth[current] = VISITING;
            match parent_dir[dir_slot[current] as usize] {
                TOP => break 0,
                LOST => break LOST,
                p => match depth[p as usize] {
                    UNSET => current = p as usize,
                    VISITING | LOST => break LOST,
                    d => break d + 1,
                },
            }
        };
        for (i, &d) in chain.iter().rev().enumerate() {
            depth[d] = if base == LOST { LOST } else { base + to_u32(i) };
        }
    }
    depth
}

/// Reachable folder numbers sorted by depth (counting sort).
fn by_depth(depth: &[u32]) -> Vec<u32> {
    let max = depth.iter().copied().filter(|&d| d != LOST).max();
    let Some(max) = max else {
        return Vec::new();
    };
    let mut start = vec![0usize; max as usize + 2];
    for &d in depth.iter().filter(|&&d| d != LOST) {
        start[d as usize + 1] += 1;
    }
    for i in 1..start.len() {
        start[i] += start[i - 1];
    }
    let mut order = vec![0u32; start[max as usize + 1]];
    for (folder, &d) in depth.iter().enumerate() {
        if d != LOST {
            order[start[d as usize]] = to_u32(folder);
            start[d as usize] += 1;
        }
    }
    order
}

/// Slot counts are bounded by u32 (see `Index::alloc_slot`).
fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn slots(n: usize) -> impl Iterator<Item = u32> {
    slots_in(0, n)
}

fn slots_in(start: usize, end: usize) -> impl Iterator<Item = u32> {
    (start..end).map(to_u32)
}

/// Text after the last dot; empty without a dot or for names like `.gitignore`.
fn extension(name: &[u8]) -> &[u8] {
    match memchr::memrchr(b'.', name) {
        Some(pos) if pos > 0 => &name[pos + 1..],
        _ => &[],
    }
}

/// Extension -> (bytes, allocated bytes, files), keyed by slices of the folded pool.
type ExtMap<'a> = HashMap<&'a [u8], (u64, u64, u64), BuildHasherDefault<Fnv>>;

/// FNV-1a: extensions are short, and SipHash's DoS protection is not needed here.
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FoldOptions;
    use crate::index::{Name, NewEntry};
    use crate::query::{Query, match_mask};

    const DIR: u32 = 0x10;

    fn add(index: &mut Index, frn: u64, parent: u64, name: &str, dir: bool, size: u64) {
        index
            .upsert(&NewEntry {
                frn,
                parent_frn: parent,
                name: Name::Utf8(name),
                attributes: if dir { DIR } else { 0x20 },
                size,
                alloc_size: size.div_ceil(4096) * 4096,
                mtime: frn,
            })
            .unwrap();
    }

    /// C:\
    ///   Users\me\Videos\a.mp4 (1000), b.MKV (3000)
    ///   Users\me\notes.txt (10)
    ///   Empty\
    ///   pagefile.sys (500)
    ///   orphan.bin (parent 99 unknown, 7)
    fn sample() -> Index {
        let mut index = Index::new("C:\\", FoldOptions::default());
        add(&mut index, 30, 22, "a.mp4", false, 1000); // children before parents on purpose
        add(&mut index, 31, 22, "b.MKV", false, 3000);
        add(&mut index, 22, 21, "Videos", true, 0);
        add(&mut index, 23, 21, "notes.txt", false, 10);
        add(&mut index, 21, 20, "me", true, 0);
        add(&mut index, 20, 5, "Users", true, 0);
        add(&mut index, 40, 5, "Empty", true, 0);
        add(&mut index, 41, 5, "pagefile.sys", false, 500);
        add(&mut index, 50, 99, "orphan.bin", false, 7);
        index
    }

    fn names(index: &Index, totals: &SizeTotals, folder: Option<u32>) -> Vec<String> {
        totals
            .children(index, folder)
            .iter()
            .map(|&s| index.name(s).to_owned())
            .collect()
    }

    #[test]
    fn totals_aggregate_bottom_up() {
        let index = sample();
        let totals = SizeTotals::build(&index, None);
        let root = totals.root();
        assert_eq!((root.size, root.files, root.folders), (4510, 4, 4));
        assert_eq!(root.alloc_size, 4096 * 4);
        assert_eq!(root.mtime, 41);
        assert_eq!(
            names(&index, &totals, None),
            ["Users", "pagefile.sys", "Empty"]
        );
        assert_eq!(totals.child_count(&index, None), 3);

        let users = index.lookup(20).unwrap();
        let t = totals.totals(&index, users);
        assert_eq!((t.size, t.files, t.folders, t.mtime), (4010, 3, 2, 31));
        let me = totals.resolve(&index, r"users\ME").unwrap().unwrap();
        assert_eq!(names(&index, &totals, Some(me)), ["Videos", "notes.txt"]);
        assert_eq!(totals.resolve(&index, ""), Some(None));
        assert_eq!(totals.resolve(&index, r"Users\nobody"), None);
        assert!(totals.children(&index, index.lookup(40)).is_empty());
        let notes = index.lookup(23).unwrap();
        assert_eq!(totals.totals(&index, notes).size, 10);
        assert!(totals.children(&index, Some(notes)).is_empty());
    }

    #[test]
    fn filter_hides_folders_without_matches() {
        let index = sample();
        let query = Query::parse("ext:mp4;mkv", FoldOptions::default()).unwrap();
        let totals = SizeTotals::build(&index, Some(match_mask(&index, &query)));
        let root = totals.root();
        assert_eq!((root.size, root.files, root.folders), (4000, 2, 3));
        assert_eq!(names(&index, &totals, None), ["Users"]);
        let me = totals.resolve(&index, r"Users\me").unwrap();
        assert_eq!(names(&index, &totals, me), ["Videos"]);
        assert!(totals.resolve(&index, r"Users\me\notes.txt").is_none());
        assert_eq!(totals.largest_files(&index, None, 10).len(), 2);
    }

    #[test]
    fn largest_files_and_extensions() {
        let index = sample();
        let totals = SizeTotals::build(&index, None);
        let largest: Vec<_> = totals
            .largest_files(&index, None, 3)
            .into_iter()
            .map(|s| index.name(s))
            .collect();
        assert_eq!(largest, ["b.MKV", "a.mp4", "pagefile.sys"]);
        let users = totals.resolve(&index, "Users").unwrap();
        assert_eq!(totals.largest_files(&index, users, 10).len(), 3);
        let videos = totals.resolve(&index, r"Users\me\Videos").unwrap();
        assert_eq!(totals.largest_files(&index, videos, 10).len(), 2);
        assert!(
            totals
                .largest_files(&index, index.lookup(40), 10)
                .is_empty()
        );

        let ext = totals.extensions(&index, None, 3);
        let summary: Vec<_> = ext.iter().map(|e| (e.ext.as_str(), e.size)).collect();
        assert_eq!(summary, [("mkv", 3000), ("mp4", 1000), ("sys", 500)]);
        let ext = totals.extensions(&index, users, 10);
        assert_eq!(ext.len(), 3);
        assert_eq!(extension(b".gitignore"), b"");
        assert_eq!(extension(b"archive.tar.gz"), b"gz");
    }

    #[test]
    fn sibling_subtrees_do_not_overlap() {
        let mut index = Index::new("C:\\", FoldOptions::default());
        add(&mut index, 20, 5, "a", true, 0);
        add(&mut index, 21, 20, "a1", true, 0);
        add(&mut index, 22, 21, "deep.bin", false, 5);
        add(&mut index, 30, 5, "b", true, 0);
        add(&mut index, 31, 30, "b.bin", false, 7);
        add(&mut index, 32, 20, "a.bin", false, 3);
        let totals = SizeTotals::build(&index, None);
        let a = totals.resolve(&index, "a").unwrap();
        let b = totals.resolve(&index, "b").unwrap();
        let a1 = totals.resolve(&index, r"a\a1").unwrap();
        let sizes = |folder| {
            totals
                .largest_files(&index, folder, 10)
                .iter()
                .map(|&s| index.size(s))
                .collect::<Vec<_>>()
        };
        assert_eq!(sizes(a), [5, 3]);
        assert_eq!(sizes(b), [7]);
        assert_eq!(sizes(a1), [5]);
        assert_eq!(sizes(None), [7, 5, 3]);
    }

    #[test]
    fn removed_entries_and_cycles_are_ignored() {
        let mut index = sample();
        index.remove(31);
        add(&mut index, 60, 61, "x", true, 0); // 60 <-> 61 cycle, unreachable
        add(&mut index, 61, 60, "y", true, 0);
        add(&mut index, 62, 61, "lost.bin", false, 99);
        let totals = SizeTotals::build(&index, None);
        assert_eq!(totals.root().size, 1510);
        assert_eq!(totals.largest_files(&index, None, 10).len(), 3);
        assert!(totals.heap_bytes() > 0);
    }
}
