//! Development stand-in for ferro-service that needs no administrator rights: walks a
//! folder with `std::fs`, indexes it (with real sizes and dates) and serves the real pipe
//! protocol. Run the UI against it with `FERROFS_DEV_ANY_SERVER=1`:
//!
//! ```text
//! cargo run -p ferro-search --example fake_service -- C:\some\folder
//! $env:FERROFS_DEV_ANY_SERVER = 1; cargo run -p ferro-search -- --show
//! ```

use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};

use ferro_core::{
    FoldOptions, Index, Name, NewEntry, Query, SearchOptions, SortBy, search_volumes,
};
use ferro_ipc::{
    ErrorCode, PIPE_NAME, PipeServer, Request, Response, Row, SearchResponse, Sort, Stats,
};

/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01, in 100 ns units.
const UNIX_TO_FILETIME: u64 = 116_444_736_000_000_000;

fn walk(index: &mut Index, dir: &Path, parent: u64, next: &mut u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let frn = *next;
        *next += 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| UNIX_TO_FILETIME + d.as_secs() * 10_000_000);
        index.upsert(&NewEntry {
            frn,
            parent_frn: parent,
            name: Name::Utf8(&name),
            attributes: if meta.is_dir() { 0x10 } else { 0x20 },
            size: if meta.is_dir() { 0 } else { meta.len() },
            alloc_size: 0,
            mtime,
        });
        if meta.is_dir() && !meta.file_type().is_symlink() {
            walk(index, &entry.path(), frn, next);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let root = std::fs::canonicalize(&root)?;
    let display = root.display().to_string();
    let display = display
        .trim_start_matches(r"\\?\")
        .trim_end_matches('\\')
        .to_owned()
        + "\\";

    let started = Instant::now();
    let mut index = Index::new(display.clone(), FoldOptions::default());
    let mut next = 16;
    walk(&mut index, &root, 5, &mut next);
    println!(
        "indexed {} entries under {display} in {:?}",
        index.len(),
        started.elapsed()
    );

    let handler = |request: Request| match request {
        Request::Search(search) => {
            let query = match Query::parse(&search.query, FoldOptions::default()) {
                Ok(query) => query,
                Err(err) => {
                    return Response::Error {
                        code: ErrorCode::BadQuery,
                        message: err.to_string(),
                    };
                }
            };
            let options = SearchOptions {
                sort: match search.sort {
                    Sort::Relevance => SortBy::Relevance,
                    Sort::Name => SortBy::Name,
                    Sort::Size => SortBy::Size,
                    Sort::Modified => SortBy::Modified,
                },
                descending: search.descending,
                offset: search.offset as usize,
                limit: search.limit as usize,
            };
            let started = Instant::now();
            let result = search_volumes(&[&index], &query, &options);
            Response::Search(SearchResponse {
                rows: result
                    .hits
                    .iter()
                    .map(|h| Row {
                        path: index.path(h.slot),
                        size: index.size(h.slot),
                        mtime: index.mtime(h.slot),
                        flags: index.flags(h.slot),
                    })
                    .collect(),
                total: result.total as u64,
                took_us: started.elapsed().as_micros() as u64,
                complete: true,
            })
        }
        Request::Stats => Response::Stats(Stats {
            volumes: 1,
            entries: index.len() as u64,
            memory_bytes: index.heap_bytes() as u64,
            uptime_secs: 0,
        }),
        _ => Response::Error {
            code: ErrorCode::BadRequest,
            message: "not supported".into(),
        },
    };

    // Interactive users may create instances too, because this runs unelevated.
    let server = PipeServer::with_sddl(PIPE_NAME, "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)")?;
    println!("serving {PIPE_NAME}; Ctrl+C to stop");
    server.serve("fake", &handler)?;
    Ok(())
}
