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
    // FERROFS_SCAN_THREADS overrides the number of enumeration threads (for comparisons).
    let (index, journal) = match std::env::var("FERROFS_SCAN_THREADS")
        .ok()
        .and_then(|t| t.parse().ok())
    {
        Some(threads) => usn::scan_with_threads(&root, FoldOptions::default(), threads)?,
        None => usn::scan(&root, FoldOptions::default())?,
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

    for text in args {
        let query = Query::parse(&text, FoldOptions::default())?;
        let started = Instant::now();
        let result = search(&index, &query, 5);
        let took = started.elapsed();
        println!("\nquery {text:?}: {} matches in {took:.2?}", result.total);
        for slot in result.slots {
            println!("  {}", index.path(slot));
        }
    }
    Ok(())
}
