//! Worker thread: owns the COM apartment and performs pick/drop actions.

use std::marker::PhantomData;
use std::sync::mpsc::Receiver;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};

use crate::hook::HotkeyAction;
use crate::tray::{Level, Notifier};

/// Messages sent to the worker.
#[derive(Debug)]
pub enum Command {
    Hotkey(HotkeyAction),
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

    for command in rx {
        match command {
            Command::Shutdown => break,
            Command::Hotkey(HotkeyAction::Pick) => {
                tracing::info!("pick requested");
                notifier.notify(
                    Level::Info,
                    "Pick",
                    "Ctrl+B received. Reading the Explorer selection is not implemented yet.",
                );
            }
            Command::Hotkey(HotkeyAction::Drop { choose_mode }) => {
                tracing::info!(choose_mode, "drop requested");
                notifier.notify(
                    Level::Info,
                    "Drop",
                    "Ctrl+G received. Creating links is not implemented yet.",
                );
            }
        }
    }
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
