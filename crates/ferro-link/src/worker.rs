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
use crate::link::{self, Modes};
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
        if choose_mode {
            // TODO(link): Shift+Ctrl+G mode popup (SPEC §1.4); uses defaults for now.
            tracing::info!("mode chooser not implemented yet; using default modes");
        }
        self.drop_into(&target, Modes::default());
    }

    fn drop_into(&self, target: &Path, modes: Modes) {
        let mut created = Vec::new();
        let mut failures = Vec::new();
        for source in &self.picked {
            match link::link_into(source, target, modes) {
                Ok(link) => {
                    tracing::info!(source = %source.display(), link = %link.link.display(), mode = %link.mode, "link created");
                    created.push(link);
                }
                Err(err) => {
                    tracing::warn!(source = %source.display(), target = %target.display(), "link failed: {err}");
                    failures.push((source, err));
                }
            }
        }

        let folder = display_name(target);
        match (created.as_slice(), failures.as_slice()) {
            ([one], []) => self.notifier.notify(
                Level::Info,
                "Linked",
                &format!("{} ({}) in {folder}", display_name(&one.link), one.mode),
            ),
            (many, []) => self.notifier.notify(
                Level::Info,
                "Linked",
                &format!("{} items in {folder}", many.len()),
            ),
            (done, [(source, err), rest @ ..]) => {
                let title = if done.is_empty() {
                    "Link failed"
                } else {
                    "Some links failed"
                };
                let mut body = format!("{}: {err}", display_name(source));
                if !rest.is_empty() {
                    body.push_str(&format!(" (+{} more failed)", rest.len()));
                }
                if !done.is_empty() {
                    body.push_str(&format!(". {} linked.", done.len()));
                }
                let level = if done.is_empty() {
                    Level::Error
                } else {
                    Level::Warning
                };
                self.notifier.notify(level, title, &body);
            }
        }
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
