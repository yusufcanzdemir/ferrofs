//! Worker thread: owns the COM apartment and performs pick/drop actions.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};

use crate::hook::HotkeyAction;
use crate::selection::{self, ExplorerView};
use crate::tray::{Level, Notifier};

/// Messages sent to the worker.
#[derive(Debug)]
pub enum Command {
    /// A hotkey was pressed while `window` (an HWND as integer) had focus.
    Hotkey {
        action: HotkeyAction,
        window: isize,
    },
    Shutdown,
}

pub fn spawn(rx: Receiver<Command>, notifier: Notifier) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("worker".into())
        .spawn(move || run(&rx, notifier))
        .context("cannot spawn worker thread")
}

fn run(rx: &Receiver<Command>, notifier: Notifier) {
    let _com = match ComApartment::init() {
        Ok(com) => com,
        Err(err) => {
            tracing::error!("cannot initialise COM: {err:#}");
            notifier.notify(Level::Error, "ferro-link", "Failed to initialise COM.");
            return;
        }
    };

    let mut worker = Worker {
        notifier,
        picked: Vec::new(),
    };
    for command in rx {
        match command {
            Command::Shutdown => break,
            Command::Hotkey { action, window } => {
                let window = HWND(window as *mut c_void);
                match action {
                    HotkeyAction::Pick => worker.pick(window),
                    HotkeyAction::Drop { choose_mode } => worker.drop_links(window, choose_mode),
                }
            }
        }
    }
}

struct Worker {
    notifier: Notifier,
    /// Items remembered by the last successful pick.
    picked: Vec<PathBuf>,
}

impl Worker {
    fn pick(&mut self, window: HWND) {
        let view = match selection::read(window) {
            Ok(view) => view,
            Err(err) => {
                tracing::error!("cannot read Explorer selection: {err:#}");
                self.notifier
                    .notify(Level::Error, "Pick failed", &format!("{err:#}"));
                return;
            }
        };
        tracing::info!(?view, "pick");

        if view.selection.is_empty() {
            let body = if view.virtual_items > 0 {
                "The selected items are not regular files or folders."
            } else {
                "Nothing is selected."
            };
            self.notifier.notify(Level::Warning, "Nothing picked", body);
            return;
        }

        let mut body = match view.selection.as_slice() {
            [single] => display_name(single),
            many => format!("{} items", many.len()),
        };
        if view.virtual_items > 0 {
            body.push_str(&format!(
                " ({} virtual item(s) skipped)",
                view.virtual_items
            ));
        }
        self.picked = view.selection;
        self.notifier.notify(Level::Info, "Picked", &body);
    }

    fn drop_links(&mut self, window: HWND, choose_mode: bool) {
        if self.picked.is_empty() {
            self.notifier.notify(
                Level::Warning,
                "Nothing to drop",
                "Select items and press Ctrl+B first.",
            );
            return;
        }
        let target = match selection::read(window) {
            Ok(ExplorerView {
                folder: Some(folder),
                ..
            }) => folder,
            Ok(_) => {
                self.notifier.notify(
                    Level::Warning,
                    "Cannot drop here",
                    "This location is not a regular folder.",
                );
                return;
            }
            Err(err) => {
                tracing::error!("cannot read Explorer folder: {err:#}");
                self.notifier
                    .notify(Level::Error, "Drop failed", &format!("{err:#}"));
                return;
            }
        };
        tracing::info!(?target, choose_mode, items = self.picked.len(), "drop");
        // TODO(link): create the links (next step).
        self.notifier.notify(
            Level::Info,
            "Drop",
            &format!(
                "Would link {} item(s) into {}. Link creation is not implemented yet.",
                self.picked.len(),
                display_name(&target)
            ),
        );
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Single-threaded COM apartment for the current thread; uninitialised on drop.
#[derive(Debug)]
struct ComApartment(PhantomData<*const ()>);

impl ComApartment {
    fn init() -> Result<Self> {
        // SAFETY: called once on this thread; balanced by CoUninitialize in Drop.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
            .ok()
            .context("CoInitializeEx failed")?;
        Ok(Self(PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: matches the successful CoInitializeEx in `init` on the same thread
        // (the raw-pointer marker makes the type !Send, so it cannot leave the thread).
        unsafe { CoUninitialize() };
    }
}
