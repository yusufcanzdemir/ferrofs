//! Storage analyzer (SPEC §5.1): folder sizes from the ferrofs index as a tree table and a
//! treemap, with the largest files and a per-extension breakdown.
//!
//! ```text
//! ferro-storage [folder]    open the analyzer (at `folder`, default: all volumes)
//! ```

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod format;
mod logging;
mod shell;
mod tree;
mod treemap;
mod worker;

use anyhow::{Result, anyhow};
use eframe::egui;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

const TITLE: &str = "ferrofs storage";

fn main() -> Result<()> {
    let _log = logging::init()?;
    // Trailing quotes are trimmed: `"C:\"` arrives as `C:"` because `\"` escapes the quote.
    let start = std::env::args()
        .nth(1)
        .map(|arg| arg.trim().trim_matches('"').to_owned())
        .unwrap_or_default();

    // Shell calls (reveal in Explorer) need an STA on the UI thread. S_FALSE and
    // RPC_E_CHANGED_MODE are fine: some apartment already exists.
    // SAFETY: called once on the main thread before any window exists.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([1360.0, 820.0])
            .with_min_inner_size([800.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            let (jobs, outcomes) = worker::spawn(cc.egui_ctx.clone())?;
            Ok(Box::new(app::App::new(jobs, outcomes, start)))
        }),
    )
    .map_err(|err| anyhow!("{err}"))
}
