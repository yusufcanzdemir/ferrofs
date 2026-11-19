//! Tray app that creates symlinks, junctions and hard links via hotkeys.
//!
//! Threads:
//! - main: hidden window + tray icon + menus + message loop,
//! - `keyboard-hook`: low-level keyboard hook with its own message loop,
//! - `worker`: COM (STA) work triggered by hotkeys.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod hook;
mod hotkey;
mod icon;
mod link;
mod logging;
mod paths;
mod selection;
mod single_instance;
mod tray;
mod wide;
mod worker;

use std::sync::mpsc;

use anyhow::Result;

use crate::config::Config;
use crate::tray::Level;

fn main() -> Result<()> {
    let _log_guard = logging::init()?;

    let Some(_instance) = single_instance::acquire()? else {
        tracing::info!("another ferro-link instance is already running; exiting");
        return Ok(());
    };

    let config_path = paths::config_file()?;
    let (config, config_error) = match config::load_or_create(&config_path) {
        Ok(config) => (config, None),
        Err(err) => (Config::default(), Some(err)),
    };
    hook::set_hotkeys(config.hotkeys.pick, config.hotkeys.drop);

    let tray = tray::Tray::new()?;
    let handle = tray.handle();
    if let Some(err) = config_error {
        tracing::error!("invalid config, using defaults: {err:#}");
        handle.notify(
            Level::Warning,
            "Invalid config",
            &format!("{err:#}. Using default settings."),
        );
    }

    let (tx, rx) = mpsc::channel();
    tray.connect_worker(tx.clone());
    let worker = worker::spawn(rx, handle, config, config_path)?;
    let hook = hook::spawn(tx.clone())?;
    tracing::info!(?config, "ferro-link started");

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
