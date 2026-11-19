//! Raw MFT scan on the throwaway test VHD, checked against USN enumeration and against
//! the sizes and timestamps Windows reports. Run elevated (see tests/fixtures):
//! `cargo test -p ferro-core --test mft_vhd -- --ignored`

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration test")]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Instant, UNIX_EPOCH};

use ferro_core::volume::volume_info;
use ferro_core::{FoldOptions, Index, Query, mft, search, usn};

fn test_root() -> String {
    let root = std::env::var("FERROFS_TEST_VOLUME")
        .expect("set FERROFS_TEST_VOLUME to the VHD root, e.g. T:\\ (see tests/fixtures)");
    assert_eq!(
        volume_info(&root).unwrap().label,
        "FERROTEST",
        "refusing to run on a volume that is not the test VHD"
    );
    root
}

/// Record number -> path of every entry.
fn paths(index: &Index) -> BTreeMap<u64, String> {
    (0..index.slot_count())
        .filter_map(|slot| u32::try_from(slot).ok())
        .filter(|&slot| index.is_live(slot))
        .map(|slot| (index.frn(slot) & 0x0000_FFFF_FFFF_FFFF, index.path(slot)))
        .collect()
}

fn find_one(index: &Index, name: &str) -> u32 {
    let query = Query::parse(&format!("\"{name}\""), FoldOptions::default()).unwrap();
    let result = search(index, &query, 10);
    assert_eq!(result.total, 1, "{name}");
    result.slots[0]
}

/// FILETIME of a std timestamp.
fn filetime(time: std::time::SystemTime) -> u64 {
    let since_unix = time.duration_since(UNIX_EPOCH).unwrap();
    116_444_736_000_000_000 + u64::try_from(since_unix.as_nanos() / 100).unwrap()
}

#[test]
#[ignore = "needs an elevated shell and the test VHD"]
fn mft_scan_matches_usn_and_filesystem() {
    let root = test_root();
    let base = PathBuf::from(&root).join(format!("mft-{}", std::process::id()));
    let many = base.join("many");
    fs::create_dir_all(&many).unwrap();
    for i in 0..200 {
        fs::write(many.join(format!("item-{i:03}.dat")), vec![b'x'; i * 37]).unwrap();
    }
    fs::write(base.join("small-resident.txt"), b"ten bytes!").unwrap();
    fs::write(
        base.join("big-nonresident.bin"),
        vec![7u8; 3 * 1024 * 1024 + 123],
    )
    .unwrap();
    fs::write(base.join("Şehir ılık.TXT"), b"turkish").unwrap();
    // Enough hard links that NTFS moves the names into extension records.
    let links = base.join("links");
    fs::create_dir_all(&links).unwrap();
    let linked = links.join("linked-original.txt");
    fs::write(&linked, b"linked").unwrap();
    let mut link_paths = BTreeSet::from([linked.display().to_string()]);
    for i in 0..60 {
        let link = links.join(format!("a-rather-long-hard-link-name-number-{i:02}.txt"));
        fs::hard_link(&linked, &link).unwrap();
        link_paths.insert(link.display().to_string());
    }

    let started = Instant::now();
    let (from_mft, _) = mft::scan(&root, FoldOptions::default()).unwrap();
    eprintln!(
        "MFT scan: {} entries in {:?}",
        from_mft.len(),
        started.elapsed()
    );
    let (from_usn, _) = usn::scan(&root, FoldOptions::default()).unwrap();

    let (a, b) = (paths(&from_mft), paths(&from_usn));
    let records = |m: &BTreeMap<u64, String>| m.keys().copied().collect::<BTreeSet<_>>();
    let only_mft: Vec<_> = records(&a)
        .difference(&records(&b))
        .map(|r| &a[r])
        .collect();
    let only_usn: Vec<_> = records(&b)
        .difference(&records(&a))
        .map(|r| &b[r])
        .collect();
    assert!(
        only_mft.is_empty() && only_usn.is_empty(),
        "only MFT: {only_mft:?}\nonly USN: {only_usn:?}"
    );

    // Same name for every record, except that a hard-linked file may be listed under any
    // of its names.
    for (record, path) in &a {
        if b[record] != *path {
            assert!(
                link_paths.contains(path) && link_paths.contains(&b[record]),
                "record {record}: MFT {path} vs USN {}",
                b[record]
            );
        }
    }

    let cluster = 4096; // test VHD is formatted with the default cluster size
    for name in [
        "small-resident.txt",
        "big-nonresident.bin",
        "Şehir ılık.TXT",
        "item-150.dat",
    ] {
        let path = if name.starts_with("item") {
            many.join(name)
        } else {
            base.join(name)
        };
        let meta = fs::metadata(&path).unwrap();
        let slot = find_one(&from_mft, name);
        assert_eq!(from_mft.path(slot), path.display().to_string());
        assert_eq!(from_mft.size(slot), meta.len(), "{name} size");
        assert_eq!(
            from_mft.mtime(slot),
            filetime(meta.modified().unwrap()),
            "{name} mtime"
        );
        let alloc = from_mft.alloc_size(slot);
        assert!(
            alloc == 0 || (alloc >= meta.len() && alloc % cluster == 0),
            "{name} alloc {alloc}"
        );
    }
    assert_eq!(
        from_mft.alloc_size(find_one(&from_mft, "small-resident.txt")),
        0
    );
    assert!(from_mft.is_dir(find_one(&from_mft, "many")));

    fs::remove_dir_all(&base).unwrap();
}
