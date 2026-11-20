//! Development stand-in for ferro-service that needs no administrator rights: walks a
//! folder with `std::fs`, indexes it (with real sizes and dates) and serves it through the
//! real [`Engine`] over the pipe. Run a UI against it with `FERROFS_DEV_ANY_SERVER=1`:
//!
//! ```text
//! cargo run -p ferro-service --example fake_service -- C:\some\folder
//! $env:FERROFS_DEV_ANY_SERVER = 1; cargo run -p ferro-search -- --show
//! $env:FERROFS_DEV_ANY_SERVER = 1; cargo run -p ferro-storage
//! ```
//!
//! The folder acts as the volume root, so paths in requests start with it.

use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};

use ferro_core::volume::VolumeInfo;
use ferro_core::{FoldOptions, Index, Name, NewEntry};
use ferro_ipc::{PipeServer, pipe_name};
use ferro_service::engine::Engine;

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
        let size = if meta.is_dir() { 0 } else { meta.len() };
        index.upsert(&NewEntry {
            frn,
            parent_frn: parent,
            name: Name::Utf8(&name),
            attributes: if meta.is_dir() { 0x10 } else { 0x20 },
            size,
            // Rough: whole 4 KB clusters (std::fs does not expose the real allocation).
            alloc_size: size.div_ceil(4096) * 4096,
            mtime,
        });
        if meta.is_dir() && !meta.file_type().is_symlink() {
            walk(index, &entry.path(), frn, next);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    // Absolute but not canonical: a `subst` drive stays `X:\` instead of turning into the
    // folder behind it (used for demo screenshots that must not show real paths).
    let root = std::path::absolute(&root)?;
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
    let volume = VolumeInfo {
        root: display.clone(),
        guid_path: format!(r"\\?\fake{{{display}}}\"),
        filesystem: "NTFS".into(),
        label: "fake".into(),
        serial: 0,
    };
    let engine = Engine::with_indexes(vec![(volume, index)], FoldOptions::default());

    // Interactive users may create instances too, because this runs unelevated.
    // FERROFS_DEV_PIPE moves it off the installed service's pipe.
    let pipe = pipe_name();
    let server = PipeServer::with_sddl(&pipe, "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)")?;
    println!("serving {pipe}; Ctrl+C to stop");
    server.serve("fake", &|request| engine.handle(request))?;
    Ok(())
}
