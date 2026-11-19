//! Read-only cold scan of a volume, then sample queries. Needs an elevated shell.
//!
//! ```text
//! cargo run -p ferro-core --release --example scan -- C:\ report "ext:pdf" "*.rs"
//! $env:FERROFS_SCAN_THREADS = 1   # optional: compare with single-threaded enumeration
//! ```

use std::time::Instant;

use ferro_core::{FoldOptions, Query, search, usn};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| "C:\\".to_owned());

    let started = Instant::now();
    // FERROFS_SCAN_MODE=usn forces USN enumeration; FERROFS_SCAN_THREADS sets its threads.
    let mode = std::env::var("FERROFS_SCAN_MODE").unwrap_or_default();
    let (index, journal) = if mode != "usn" {
        ferro_core::scan_volume(&root, FoldOptions::default())?
    } else {
        match std::env::var("FERROFS_SCAN_THREADS")
            .ok()
            .and_then(|t| t.parse().ok())
        {
            Some(threads) => usn::scan_with_threads(&root, FoldOptions::default(), threads)?,
            None => usn::scan(&root, FoldOptions::default())?,
        }
    };
    let elapsed = started.elapsed();
    let entries = index.len().max(1) as f64;
    println!(
        "volume      {root} (journal {:#x}, next usn {})",
        journal.journal_id, journal.next_usn
    );
    println!("entries     {}", index.len());
    println!("scan time   {elapsed:.2?}");
    println!(
        "memory      {:.1} MB total, {:.1} bytes/entry without names",
        index.heap_bytes() as f64 / 1e6,
        index.heap_bytes_without_names() as f64 / entries
    );

    // FERROFS_SCAN_COMPARE=1: also enumerate through USN and list records only one side has.
    if std::env::var("FERROFS_SCAN_COMPARE").as_deref() == Ok("1") {
        let (other, _) = usn::scan(&root, FoldOptions::default())?;
        let records = |index: &ferro_core::Index| {
            (0..index.slot_count() as u32)
                .filter(|&s| index.is_live(s))
                .map(|s| (index.frn(s) & 0x0000_FFFF_FFFF_FFFF, index.path(s)))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let (mine, theirs) = (records(&index), records(&other));
        let only_here: Vec<_> = mine
            .iter()
            .filter(|(r, _)| !theirs.contains_key(r))
            .collect();
        let only_usn: Vec<_> = theirs
            .iter()
            .filter(|(r, _)| !mine.contains_key(r))
            .collect();
        println!(
            "compare     only in this scan: {}, only in USN: {}",
            only_here.len(),
            only_usn.len()
        );
        for (record, path) in only_here.iter().take(8) {
            println!("  this-only {record:>9} {path}");
        }
        for (record, path) in only_usn.iter().take(8) {
            println!("  usn-only  {record:>9} {path}");
        }
    }

    for text in args {
        let query = Query::parse(&text, FoldOptions::default())?;
        let started = Instant::now();
        let result = search(&index, &query, 5);
        let took = started.elapsed();
        println!("\nquery {text:?}: {} matches in {took:.2?}", result.total);
        for slot in result.slots {
            println!("  {:>14} {}", index.size(slot), index.path(slot));
        }
    }
    Ok(())
}
