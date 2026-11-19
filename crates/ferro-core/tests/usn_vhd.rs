//! USN enumeration and live updates on a throwaway VHD (never a real user volume).
//!
//! Run from an elevated shell:
//! ```text
//! $l = .\tests\fixtures\new-test-vhd.ps1
//! $env:FERROFS_TEST_VOLUME = "${l}:\"
//! cargo test -p ferro-core --test usn_vhd -- --ignored
//! .\tests\fixtures\remove-test-vhd.ps1
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration test")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferro_core::usn::{self, JournalReader};
use ferro_core::volume::{Volume, volume_info};
use ferro_core::{FoldOptions, Index, Query, search};

fn test_root() -> String {
    let root = std::env::var("FERROFS_TEST_VOLUME")
        .expect("set FERROFS_TEST_VOLUME to the VHD root, e.g. T:\\ (see tests/fixtures)");
    let info = volume_info(&root).unwrap();
    assert_eq!(
        info.label, "FERROTEST",
        "refusing to run on a volume that is not the test VHD"
    );
    root
}

fn find(index: &Index, query: &str) -> Vec<String> {
    let query = Query::parse(query, index.fold_options()).unwrap();
    let mut paths: Vec<String> = search(index, &query, 10_000)
        .slots
        .into_iter()
        .map(|s| index.path(s))
        .collect();
    paths.sort();
    paths
}

fn wait_until(reader: &mut JournalReader, index: &mut Index, done: impl Fn(&Index) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(index) {
        assert!(
            Instant::now() < deadline,
            "journal did not deliver the expected changes"
        );
        reader
            .read(Duration::from_secs(1), |record| {
                usn::apply(index, record);
            })
            .unwrap();
    }
}

#[test]
#[ignore = "needs an elevated shell and the test VHD"]
fn scan_and_follow_the_journal() {
    let root = test_root();
    let base = PathBuf::from(&root).join(format!("run-{}", std::process::id()));
    let deep = base.join("alpha").join("beta");
    fs::create_dir_all(&deep).unwrap();
    for i in 0..500 {
        fs::write(deep.join(format!("file-{i:03}.txt")), b"x").unwrap();
    }
    fs::write(base.join("Şehir-ı.TXT"), b"x").unwrap();

    // Full scan.
    let started = Instant::now();
    let (mut index, journal) = usn::scan(&root, FoldOptions::default()).unwrap();
    eprintln!("scan: {} entries in {:?}", index.len(), started.elapsed());
    let base_str = base.display().to_string();
    let files = find(&index, &format!("file- path:\"{base_str}\""));
    assert_eq!(files.len(), 500);
    assert_eq!(files[0], deep.join("file-000.txt").display().to_string());
    assert_eq!(
        find(&index, "şehir"),
        [base.join("Şehir-ı.TXT").display().to_string()]
    );

    // Live updates: create, rename into another folder, delete.
    let mut reader = JournalReader::open(&root, journal).unwrap();
    fs::write(base.join("created-later.log"), b"x").unwrap();
    fs::rename(deep.join("file-001.txt"), base.join("moved.txt")).unwrap();
    fs::remove_file(deep.join("file-002.txt")).unwrap();
    let path_of = |index: &Index, q: &str| find(index, q);
    let expected_moved = base.join("moved.txt").display().to_string();
    wait_until(&mut reader, &mut index, |index| {
        path_of(index, "created-later") == [base.join("created-later.log").display().to_string()]
            && path_of(index, "moved.txt") == [expected_moved.clone()]
            && path_of(index, "file-002").is_empty()
            && path_of(index, "file-001").is_empty()
    });

    // With nothing happening, a read returns after its timeout instead of blocking forever.
    let started = Instant::now();
    let mut drained = 1;
    while drained > 0 && started.elapsed() < Duration::from_secs(10) {
        drained = reader.read(Duration::from_secs(1), |_| {}).unwrap();
    }
    assert_eq!(drained, 0, "journal kept producing records");
    assert!(started.elapsed() < Duration::from_secs(10));

    fs::remove_dir_all(&base).unwrap();
}

#[test]
#[ignore = "needs an elevated shell and the test VHD"]
fn journal_position_survives_reopen() {
    let root = test_root();
    let volume = Volume::open(&root).unwrap();
    let first = usn::query_journal(&volume).unwrap();
    let marker: &Path = &PathBuf::from(&root).join(format!("marker-{}.tmp", std::process::id()));
    fs::write(marker, b"x").unwrap();
    let second = usn::query_journal(&Volume::open(&root).unwrap()).unwrap();
    assert_eq!(first.journal_id, second.journal_id);
    assert!(second.next_usn > first.next_usn);
    fs::remove_file(marker).unwrap();
}
