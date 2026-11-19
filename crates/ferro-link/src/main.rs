//! Tray app that creates symlinks, junctions and hard links via hotkeys.
//!
//! Threads:
//! - main: hidden window + tray icon + message loop,
//! - `keyboard-hook`: low-level keyboard hook with its own message loop,
//! - `worker`: COM (STA) work triggered by hotkeys.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod hook;
mod logging;
mod paths;
mod single_instance;
mod tray;
mod wide;
mod worker;

use std::sync::mpsc;

use anyhow::Result;

fn main() -> Result<()> {
    let _log_guard = logging::init()?;

    let Some(_instance) = single_instance::acquire()? else {
        tracing::info!("another ferro-link instance is already running; exiting");
        return Ok(());
    };

    let tray = tray::Tray::new()?;
    let (tx, rx) = mpsc::channel();
    let worker = worker::spawn(rx, tray.notifier())?;
    let hook = hook::spawn(tx.clone())?;
    tracing::info!("ferro-link started");

    let result = tray.run();

    hook.stop();
    // The worker may already be gone if it failed to start; nothing to do then.
    let _ = tx.send(worker::Command::Shutdown);
    if worker.join().is_err() {
        tracing::error!("worker thread panicked");
    }
    tracing::info!("ferro-link stopped");
    result
}
