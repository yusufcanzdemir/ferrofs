//! Windows service that owns the volume indexes and serves queries over a named pipe.
//!
//! ```text
//! ferro-service install          register + start the service (elevated)
//! ferro-service uninstall        stop + remove the service (elevated)
//! ferro-service run --console    run in the foreground for debugging (elevated, Ctrl+C stops)
//! ferro-service status           volume states, via the pipe
//! ferro-service query <text>     search, via the pipe
//! ferro-service sizes [folder]   folder sizes and largest files, via the pipe
//! ferro-service dupes [folder]   equal-size duplicate candidates, via the pipe
//! ferro-service cleanup [folder] developer artifacts (build output), via the pipe
//! ```
//! Without arguments the binary expects to be started by the service control manager.

mod logging;
mod service;
mod snapshots;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ferro_core::FoldOptions;
use ferro_core::volume::fixed_volumes;
use ferro_ipc::{
    CleanupScanRequest, Client, DuplicateCandidatesRequest, PipeServer, Request, Response,
    SearchRequest, ServerTrust, SizeTreeRequest, StopSignal,
};
use windows::Win32::Foundation::{FALSE, TRUE};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::core::BOOL;

use ferro_service::engine::Engine;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] => {
            let _log = logging::init(false)?;
            service::run_dispatcher()
        }
        ["run", "--console"] => {
            let _log = logging::init(true)?;
            run_console()
        }
        ["install"] => service::install(),
        ["uninstall"] => service::uninstall(),
        ["status"] => status(),
        ["query", words @ ..] => query(&words.join(" ")),
        ["sizes", folder @ ..] => sizes(&folder.join(" ")),
        ["dupes", folder @ ..] => dupes(&folder.join(" ")),
        ["cleanup", folder @ ..] => cleanup(&folder.join(" ")),
        _ => bail!(
            "usage: ferro-service [install | uninstall | run --console | status | query <text> | sizes [folder] | dupes [folder] | cleanup [folder]]"
        ),
    }
}

/// Indexes all NTFS volumes and serves the pipe until the server is stopped.
pub fn run(server: &PipeServer) -> Result<()> {
    let volumes: Vec<_> = fixed_volumes()?
        .into_iter()
        .filter(|v| v.is_ntfs())
        .collect();
    tracing::info!(
        volumes = ?volumes.iter().map(|v| v.root.as_str()).collect::<Vec<_>>(),
        "starting"
    );
    let snapshot_dir = match snapshots::prepare_dir() {
        Ok(dir) => Some(dir),
        Err(err) => {
            tracing::warn!("index snapshots disabled: {err:#}");
            None
        }
    };
    let engine = Engine::new(volumes, FoldOptions::default(), snapshot_dir);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for i in 0..engine.volume_count() {
            let (engine, stop) = (&engine, &stop);
            std::thread::Builder::new()
                .name(format!("volume-{i}"))
                .spawn_scoped(scope, move || engine.run_volume(i, stop))
                .context("cannot spawn volume thread")?;
        }
        let served = server.serve(VERSION, &|request| engine.handle(request));
        stop.store(true, Ordering::Relaxed);
        served.context("pipe server failed")
    })
}

static CONSOLE_STOP: OnceLock<StopSignal> = OnceLock::new();
static CONSOLE_STOPPING: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_console_ctrl(_event: u32) -> BOOL {
    if CONSOLE_STOPPING.swap(true, Ordering::Relaxed) {
        return FALSE; // second Ctrl+C: let Windows terminate us
    }
    tracing::info!("Ctrl+C: stopping");
    if let Some(stop) = CONSOLE_STOP.get() {
        stop.stop();
    }
    TRUE
}

fn run_console() -> Result<()> {
    // `run --console` honours FERROFS_DEV_PIPE (the installed service never does).
    let server = PipeServer::new(&ferro_ipc::pipe_name())?;
    let _ = CONSOLE_STOP.set(server.stop_signal());
    // SAFETY: `on_console_ctrl` matches PHANDLER_ROUTINE and lives for the whole program.
    unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), true) }
        .context("cannot install Ctrl+C handler")?;
    run(&server)
}

fn connect() -> Result<Client> {
    // Debug commands also work against `run --console`, which is not in session 0.
    Client::connect(
        &ferro_ipc::pipe_name(),
        "ferro-service-cli",
        Duration::from_secs(5),
        ServerTrust::Any,
    )
    .context("cannot reach the ferrofs service")
}

fn status() -> Result<()> {
    let mut client = connect()?;
    println!("service {}", client.service_version());
    if let Response::Stats(stats) = client.request(&Request::Stats)? {
        println!(
            "{} entries, {:.1} MB, up {} s",
            stats.entries,
            stats.memory_bytes as f64 / 1e6,
            stats.uptime_secs
        );
    }
    if let Response::Volumes(volumes) = client.request(&Request::VolumeList)? {
        for v in volumes {
            println!(
                "{:<4} {:?} {} entries (usn {})",
                v.root, v.state, v.entries, v.next_usn
            );
        }
    }
    Ok(())
}

fn query(text: &str) -> Result<()> {
    let mut client = connect()?;
    match client.request(&Request::Search(SearchRequest::new(text, 20)))? {
        Response::Search(result) => {
            for row in &result.rows {
                println!("{:>14} {}", row.size, row.path);
            }
            println!(
                "-- {} matches in {:.2} ms{}",
                result.total,
                result.took_us as f64 / 1000.0,
                if result.complete {
                    ""
                } else {
                    " (indexing not finished)"
                }
            );
            Ok(())
        }
        Response::Error { code, message } => bail!("{code:?}: {message}"),
        other => bail!("unexpected response {other:?}"),
    }
}

/// Prints the size tree below `root` (empty = all volumes) and timing, for manual checks.
fn sizes(root: &str) -> Result<()> {
    let mut client = connect()?;
    let mut request = SizeTreeRequest::new(root, 1);
    request.max_children = 20;
    request.top_files = 10;
    request.top_extensions = 10;
    match client.request(&Request::SizeTree(request))? {
        Response::SizeTree(tree) => {
            let gb = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
            println!(
                "{}  {:.2} GB ({:.2} GB on disk), {} files, {} folders",
                if tree.path.is_empty() {
                    "all volumes"
                } else {
                    &tree.path
                },
                gb(tree.root.size),
                gb(tree.root.alloc_size),
                tree.root.files,
                tree.root.folders
            );
            for child in &tree.root.children {
                println!("{:>10.2} GB  {}", gb(child.size), child.name);
            }
            if let Some(rest) = &tree.root.rest {
                println!("{:>10.2} GB  ({} more)", gb(rest.size), rest.count);
            }
            println!("-- largest files");
            for row in &tree.largest {
                println!("{:>10.2} GB  {}", gb(row.size), row.path);
            }
            println!("-- extensions");
            for ext in &tree.extensions {
                println!(
                    "{:>10.2} GB  .{} ({} files)",
                    gb(ext.size),
                    ext.ext,
                    ext.files
                );
            }
            println!("-- {:.2} ms", tree.took_us as f64 / 1000.0);
            Ok(())
        }
        Response::Error { code, message } => bail!("{code:?}: {message}"),
        other => bail!("unexpected response {other:?}"),
    }
}

/// Prints equal-size candidate groups below `root` (empty = all volumes), for manual checks.
fn dupes(root: &str) -> Result<()> {
    let mut client = connect()?;
    let request = DuplicateCandidatesRequest {
        roots: if root.is_empty() {
            Vec::new()
        } else {
            vec![root.to_owned()]
        },
        min_size: 64 * 1024,
        filter: String::new(),
        max_files: 200_000,
    };
    match client.request(&Request::DuplicateCandidates(request))? {
        Response::DuplicateCandidates(result) => {
            let mb = |bytes: u64| bytes as f64 / f64::from(1u32 << 20);
            for group in result.groups.iter().take(10) {
                println!("{:>10.1} MB x {}", mb(group.size), group.files.len());
                for file in group.files.iter().take(3) {
                    println!("              {}", file.path);
                }
            }
            let listed: usize = result.groups.iter().map(|g| g.files.len()).sum();
            println!(
                "-- {} groups / {} files of equal size ({} files listed) in {:.2} ms",
                result.total_groups,
                result.total_files,
                listed,
                result.took_us as f64 / 1000.0
            );
            Ok(())
        }
        Response::Error { code, message } => bail!("{code:?}: {message}"),
        other => bail!("unexpected response {other:?}"),
    }
}

/// Prints developer artifacts below `root` (empty = all volumes), for manual checks.
fn cleanup(root: &str) -> Result<()> {
    let mut client = connect()?;
    let request = CleanupScanRequest {
        roots: if root.is_empty() {
            Vec::new()
        } else {
            vec![root.to_owned()]
        },
        include_venv: true,
        min_size: 1 << 20,
    };
    match client.request(&Request::CleanupScan(request))? {
        Response::CleanupScan(scan) => {
            let mb = |bytes: u64| bytes as f64 / f64::from(1u32 << 20);
            for artifact in scan.artifacts.iter().take(25) {
                println!(
                    "{:>10.1} MB  {:?}  {}",
                    mb(artifact.size),
                    artifact.kind,
                    artifact.path
                );
            }
            let total: u64 = scan.artifacts.iter().map(|a| a.size).sum();
            println!(
                "-- {} artifacts, {:.1} MB, in {:.2} ms",
                scan.artifacts.len(),
                mb(total),
                scan.took_us as f64 / 1000.0
            );
            Ok(())
        }
        Response::Error { code, message } => bail!("{code:?}: {message}"),
        other => bail!("unexpected response {other:?}"),
    }
}
