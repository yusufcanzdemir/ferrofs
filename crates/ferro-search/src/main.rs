//! Instant file search UI (SPEC §4): a borderless egui window toggled with Ctrl+Alt+Space
//! that queries ferro-service over the named pipe.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod format;
mod history;
mod icons;
mod logging;
mod shell;
mod window;
mod worker;

use std::sync::Arc;

use anyhow::{Result, anyhow};
use eframe::egui;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::{Owned, w};

use crate::window::WindowControl;

const TITLE: &str = "ferrofs search";

fn main() -> Result<()> {
    let _log = logging::init()?;

    // SAFETY: the name is a static literal; the handle is closed when `_instance` drops.
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\ferrofs-search")) }?;
    // SAFETY: no preconditions; read right after CreateMutexW.
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    // SAFETY: a fresh handle we own.
    let _instance: Owned<HANDLE> = unsafe { Owned::new(mutex) };
    if already_running {
        tracing::info!("ferro-search is already running");
        return Ok(());
    }

    // Shell calls (reveal in Explorer) need an STA on the UI thread. S_FALSE and
    // RPC_E_CHANGED_MODE are fine: some apartment already exists.
    // SAFETY: called once on the main thread before any window exists.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([820.0, 500.0])
            .with_min_inner_size([480.0, 240.0])
            .with_decorations(false)
            .with_taskbar(false)
            .with_always_on_top()
            // eframe makes the window visible after its first frame regardless of
            // `with_visible`, so start off-screen and inactive (no flash, no focus theft);
            // the app hides it on its first frame.
            .with_position([-32000.0, -32000.0])
            .with_active(false)
            .with_visible(false),
        ..Default::default()
    };
    let control = Arc::new(WindowControl::default());
    eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            let hwnd = window::find_own_window(TITLE);
            if let Some(hwnd) = hwnd {
                control.attach(hwnd, cc.egui_ctx.clone());
                window::round_corners(hwnd);
            } else {
                tracing::error!("cannot find the search window handle");
            }
            let hotkey = config::load_hotkey();
            let label = format!("{hotkey:?}");
            if let Err(err) = window::spawn_hotkey_thread(Arc::clone(&control), hotkey, label) {
                tracing::error!("{err:#}");
            }
            let (jobs, outcomes) = worker::spawn(cc.egui_ctx.clone())?;
            let show_now = std::env::args().any(|a| a == "--show");
            if show_now {
                control.show();
            }
            let history = history::History::load(history::History::default_file());
            Ok(Box::new(app::App::new(
                control, hwnd, jobs, outcomes, !show_now, history,
            )))
        }),
    )
    .map_err(|err| anyhow!("{err}"))
}
