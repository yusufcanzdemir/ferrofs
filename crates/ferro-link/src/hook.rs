//! Low-level keyboard hook (SPEC §1.2).
//!
//! `WH_KEYBOARD_LL` is used instead of `RegisterHotKey` so the hotkeys are only
//! consumed while Explorer or the desktop has focus; everywhere else the keys
//! pass through untouched. The callback must return quickly: it only inspects
//! window classes and modifier state, then hands the work to the worker thread.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo,
    GetMessageW, GetWindowThreadProcessId, HC_ACTION, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE,
    PeekMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use windows::core::Owned;

use crate::wide::{window_class, window_class_is};
use crate::worker::Command;

/// What a recognised hotkey asks the worker to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    /// Ctrl+B: remember the current Explorer selection.
    Pick,
    /// Ctrl+G: create links in the current Explorer folder.
    /// `choose_mode` is set when Shift is held (opens the mode popup).
    Drop { choose_mode: bool },
}

// TODO(config): make these configurable (SPEC §1.5).
const PICK_VK: u32 = b'B' as u32;
const DROP_VK: u32 = b'G' as u32;

/// Top-level windows where the hotkeys are active: Explorer and the desktop.
const EXPLORER_CLASSES: &[&str] = &["CabinetWClass", "Progman", "WorkerW"];
/// Focused child windows that show the item view (Explorer list / desktop icons).
/// Anything else (rename box, address bar, search box) lets the keys through.
const ITEM_VIEW_CLASSES: &[&str] = &["DirectUIHWND", "SysListView32"];

static SENDER: OnceLock<Sender<Command>> = OnceLock::new();
/// Virtual key whose key-down we swallowed; its repeats and key-up are swallowed too.
static SWALLOWED_VK: AtomicU32 = AtomicU32::new(0);

/// Handle to the running hook thread.
#[derive(Debug)]
pub struct HookThread {
    thread_id: u32,
    handle: JoinHandle<()>,
}

impl HookThread {
    /// Stops the message loop (which unhooks) and waits for the thread.
    pub fn stop(self) {
        // SAFETY: posting WM_QUIT to a thread we created; it owns a message queue
        // because `spawn` waits until the queue exists.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Err(err) = posted {
            tracing::error!("cannot stop keyboard hook thread: {err}");
            return;
        }
        if self.handle.join().is_err() {
            tracing::error!("keyboard hook thread panicked");
        }
    }
}

/// Installs the hook on a dedicated thread. Can only be called once per process.
pub fn spawn(sender: Sender<Command>) -> Result<HookThread> {
    SENDER
        .set(sender)
        .map_err(|_| anyhow!("keyboard hook is already installed"))?;

    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<u32>>(1);
    let handle = thread::Builder::new()
        .name("keyboard-hook".into())
        .spawn(move || hook_thread(&ready_tx))
        .context("cannot spawn keyboard hook thread")?;

    let thread_id = ready_rx
        .recv()
        .context("keyboard hook thread exited during startup")??;
    Ok(HookThread { thread_id, handle })
}

fn hook_thread(ready: &mpsc::SyncSender<Result<u32>>) {
    let hook = match install_hook() {
        Ok(hook) => hook,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    let mut msg = MSG::default();
    // Force creation of this thread's message queue before reporting ready,
    // so `PostThreadMessageW(WM_QUIT)` cannot be lost.
    // SAFETY: `msg` is a valid out-pointer.
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: no preconditions.
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
    tracing::debug!("keyboard hook installed");

    // The hook callback only runs while this thread pumps messages.
    // SAFETY: `msg` is a valid out-pointer; GetMessageW returns 0 on WM_QUIT and -1 on error.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: `msg` was filled by GetMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    drop(hook);
    tracing::debug!("keyboard hook removed");
}

fn install_hook() -> Result<Owned<windows::Win32::UI::WindowsAndMessaging::HHOOK>> {
    // SAFETY: passing None returns the handle of the current executable, which is never freed.
    let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
    // SAFETY: `keyboard_proc` matches HOOKPROC and lives for the whole program.
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_proc),
            Some(HINSTANCE::from(module)),
            0,
        )
    }
    .context("SetWindowsHookExW(WH_KEYBOARD_LL) failed")?;
    // SAFETY: the hook was just installed and is owned only here; Owned unhooks on drop.
    Ok(unsafe { Owned::new(hook) })
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, `lparam` points to a valid
        // KBDLLHOOKSTRUCT for the duration of this call.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // WM_* key messages fit in u32; truncation is intended.
        #[allow(clippy::cast_possible_truncation)]
        let message = wparam.0 as u32;
        if handle_key(message, info.vkCode) {
            return LRESULT(1);
        }
    }
    // SAFETY: forwarding the unmodified arguments to the next hook in the chain.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Returns true if the key event must be swallowed.
fn handle_key(message: u32, vk: u32) -> bool {
    match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if SWALLOWED_VK.load(Ordering::Relaxed) == vk {
                return true; // auto-repeat of a hotkey we already handled
            }
            let Some(action) = match_hotkey(vk) else {
                return false;
            };
            if !explorer_item_view_has_focus() {
                return false;
            }
            SWALLOWED_VK.store(vk, Ordering::Relaxed);
            if let Some(sender) = SENDER.get()
                && sender.send(Command::Hotkey(action)).is_err()
            {
                tracing::warn!("worker is gone; hotkey ignored");
            }
            true
        }
        WM_KEYUP | WM_SYSKEYUP => SWALLOWED_VK
            .compare_exchange(vk, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok(),
        _ => false,
    }
}

fn match_hotkey(vk: u32) -> Option<HotkeyAction> {
    if vk != PICK_VK && vk != DROP_VK {
        return None; // cheap early exit for every other key
    }
    // Ctrl+Alt is AltGr on many layouts (e.g. Turkish), so Alt must be up.
    if !is_down(VK_CONTROL) || is_down(VK_MENU) || is_down(VK_LWIN) || is_down(VK_RWIN) {
        return None;
    }
    let shift = is_down(VK_SHIFT);
    match vk {
        PICK_VK if !shift => Some(HotkeyAction::Pick),
        DROP_VK => Some(HotkeyAction::Drop { choose_mode: shift }),
        _ => None,
    }
}

fn is_down(key: VIRTUAL_KEY) -> bool {
    // SAFETY: no preconditions; the high bit of the result is the "down" state.
    let state = unsafe { GetAsyncKeyState(i32::from(key.0)) };
    state < 0
}

fn explorer_item_view_has_focus() -> bool {
    // SAFETY: no preconditions.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() || !window_class_is(foreground, EXPLORER_CLASSES) {
        return false;
    }

    // SAFETY: `foreground` is a window handle; a stale handle just yields 0.
    let thread_id = unsafe { GetWindowThreadProcessId(foreground, None) };
    let mut gui = GUITHREADINFO {
        cbSize: u32::try_from(size_of::<GUITHREADINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: `gui` is a valid out-pointer with `cbSize` initialised.
    if unsafe { GetGUIThreadInfo(thread_id, &mut gui) }.is_err() || gui.hwndFocus.is_invalid() {
        return false;
    }
    if window_class_is(gui.hwndFocus, ITEM_VIEW_CLASSES) {
        return true;
    }
    tracing::debug!(
        focus_class = %window_class(gui.hwndFocus),
        "hotkey ignored: focus is not on the item view"
    );
    false
}
