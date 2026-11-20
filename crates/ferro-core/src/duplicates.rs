//! Duplicate candidates for the storage tool (SPEC §5.2, step 1): files of equal size.
//! Hashing happens in the UI process with the user's own permissions; the index only
//! narrows the search down to files that could be identical.

use rayon::prelude::*;

use crate::index::{Index, flags};
use crate::sizes::SizeTotals;

/// Slots per parallel work item.
const CHUNK: usize = 1 << 15;

/// Files below `folder` (`None` = whole volume) that may have duplicates, as
/// `(size, slot)`: regular files of at least `min_size` bytes that match the filter the
/// totals were built with. Reparse points (their data lives elsewhere) and system files
/// are skipped. Hard links need no special care: the index holds one slot per record.
pub fn candidate_files(
    index: &Index,
    totals: &SizeTotals,
    folder: Option<u32>,
    min_size: u64,
) -> Vec<(u64, u32)> {
    let below = totals.file_below(index, folder);
    let min_size = min_size.max(1);
    (0..index.slot_count().div_ceil(CHUNK))
        .into_par_iter()
        .flat_map_iter(|chunk| {
            let end = ((chunk + 1) * CHUNK).min(index.slot_count());
            let below = &below;
            (chunk * CHUNK..end).filter_map(move |slot| {
                // Slot counts are bounded by u32 (see Index::alloc_slot).
                #[allow(clippy::cast_possible_truncation)]
                let slot = slot as u32;
                let size = index.size(slot);
                let skip = flags::REPARSE | flags::SYSTEM | flags::DIR;
                (size >= min_size && index.flags(slot) & skip == 0 && below(slot))
                    .then_some((size, slot))
            })
        })
        .collect()
}

/// Groups items by size, keeping only sizes shared by at least two distinct items.
/// Groups come out by ascending size, items in each group in ascending order.
pub fn group_by_size<T: Ord + Copy + Send>(mut items: Vec<(u64, T)>) -> Vec<(u64, Vec<T>)> {
    items.par_sort_unstable();
    items.dedup();
    let mut groups = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let size = items[start].0;
        let end = start + items[start..].partition_point(|item| item.0 == size);
        if end - start >= 2 {
            groups.push((size, items[start..end].iter().map(|item| item.1).collect()));
        }
        start = end;
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FoldOptions;
    use crate::index::{Name, NewEntry};

    fn add(index: &mut Index, frn: u64, parent: u64, name: &str, attributes: u32, size: u64) {
        index
            .upsert(&NewEntry {
                frn,
                parent_frn: parent,
                name: Name::Utf8(name),
                attributes,
                size,
                alloc_size: size,
                mtime: 0,
            })
            .unwrap();
    }

    #[test]
    fn candidates_skip_small_system_and_reparse_files() {
        let mut index = Index::new("C:\\", FoldOptions::default());
        add(&mut index, 20, 5, "a", 0x10, 0);
        add(&mut index, 21, 20, "one.bin", 0x20, 100_000);
        add(&mut index, 22, 20, "two.bin", 0x20, 100_000);
        add(&mut index, 23, 20, "small.txt", 0x20, 10);
        add(&mut index, 24, 5, "three.bin", 0x20, 100_000);
        add(&mut index, 25, 5, "pagefile.sys", 0x20 | 0x4, 100_000);
        add(&mut index, 26, 5, "link.bin", 0x20 | 0x400, 100_000);
        add(&mut index, 27, 5, "other.bin", 0x20, 200_000);
        let totals = SizeTotals::build(&index, None);

        let all = candidate_files(&index, &totals, None, 64 * 1024);
        let groups = group_by_size(all);
        assert_eq!(groups.len(), 1);
        let (size, slots) = &groups[0];
        assert_eq!(*size, 100_000);
        let mut names: Vec<_> = slots.iter().map(|&s| index.name(s)).collect();
        names.sort_unstable();
        assert_eq!(names, ["one.bin", "three.bin", "two.bin"]);

        let a = totals.resolve(&index, "a").unwrap();
        let inside = group_by_size(candidate_files(&index, &totals, a, 64 * 1024));
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].1.len(), 2);
    }

    #[test]
    fn grouping_dedups_and_drops_singletons() {
        let items = vec![(5, 'b'), (3, 'x'), (5, 'a'), (5, 'a'), (9, 'z'), (9, 'y')];
        assert_eq!(
            group_by_size(items),
            [(5, vec!['a', 'b']), (9, vec!['y', 'z'])]
        );
        assert!(group_by_size::<u32>(Vec::new()).is_empty());
    }
}
