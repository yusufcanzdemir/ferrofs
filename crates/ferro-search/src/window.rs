//! Showing, hiding and placing the search window, and the global toggle hotkey (SPEC §4).
//!
//! The window is driven directly through Win32 so it can be shown from the hotkey thread
//! while egui is idle; egui is then asked to repaint.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result};
use eframe::egui;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_NOREPEAT, RegisterHotKey};

use crate::config::Hotkey;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumThreadWindows, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, MSG, SW_HIDE, SW_SHOW, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindow, WM_HOTKEY,
};
use windows::core::BOOL;

/// Shared between the UI thread and the hotkey thread.
#[derive(Debug, Default)]
pub struct WindowControl {
    hwnd: AtomicIsize,
    visible: AtomicBool,
    shown_at: Mutex<Option<Instant>>,
    ctx: OnceLock<egui::Context>,
}

impl WindowControl {
    pub fn attach(&self, hwnd: isize, ctx: egui::Context) {
        self.hwnd.store(hwnd, Ordering::Relaxed);
        let _ = self.ctx.set(ctx);
    }

    fn hwnd(&self) -> Option<HWND> {
        match self.hwnd.load(Ordering::Relaxed) {
            0 => None,
            raw => Some(HWND(raw as *mut _)),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }

    pub fn toggle(&self) {
        if self.is_visible() {
            self.hide()
        } else {
            self.show()
        }
    }

    /// Centers the window on the monitor under the cursor, shows and focuses it.
    pub fn show(&self) {
        let Some(hwnd) = self.hwnd() else { return };
        if let Ok(mut shown_at) = self.shown_at.lock() {
            *shown_at = Some(Instant::now());
        }
        place_on_cursor_monitor(hwnd);
        // SAFETY: `hwnd` is our top-level window; showing it from another thread is allowed
        // (the call is marshalled to the owning thread).
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        bring_to_foreground(hwnd);
        self.visible.store(true, Ordering::Relaxed);
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    pub fn hide(&self) {
        let Some(hwnd) = self.hwnd() else { return };
        // SAFETY: `hwnd` is our top-level window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        self.visible.store(false, Ordering::Relaxed);
    }

    /// When the window was last shown, consumed by the first frame afterwards.
    pub fn take_shown_at(&self) -> Option<Instant> {
        self.shown_at.lock().ok().and_then(|mut t| t.take())
    }
}

/// Makes `hwnd` the foreground window with keyboard focus.
///
/// Windows often refuses `SetForegroundWindow` while another app is in front, even right
/// after our hotkey (the window then appears but typing still goes to the other app).
/// Temporarily attaching to the foreground thread's input queue lifts that restriction.
fn bring_to_foreground(hwnd: HWND) {
    // SAFETY: plain window/thread calls on valid handles; the input attachment is undone
    // before returning.
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground == hwnd {
            return;
        }
        let foreground_thread = GetWindowThreadProcessId(foreground, None);
        let this_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != this_thread
            && AttachThreadInput(this_thread, foreground_thread, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(this_thread, foreground_thread, false);
        }
        if GetForegroundWindow() != hwnd {
            tracing::warn!("could not bring the search window to the foreground");
        }
    }
}

/// Puts the window in the upper third of the work area of the monitor under the cursor.
fn place_on_cursor_monitor(hwnd: HWND) {
    let mut cursor = POINT::default();
    let mut window = RECT::default();
    let mut monitor = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: all out-pointers reference live locals; `hwnd` is our window.
    unsafe {
        if GetCursorPos(&mut cursor).is_err() || GetWindowRect(hwnd, &mut window).is_err() {
            return;
        }
        let handle = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        if !GetMonitorInfoW(handle, &mut monitor).as_bool() {
            return;
        }
        let work = monitor.rcWork;
        let (w, h) = (window.right - window.left, window.bottom - window.top);
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.top + (work.bottom - work.top - h) / 4;
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Finds the top-level window titled `title` that belongs to the calling thread.
pub fn find_own_window(title: &str) -> Option<isize> {
    struct Search {
        title: Vec<u16>,
        found: isize,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Search` passed below, alive for the whole enumeration.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut buf = [0u16; 256];
        // SAFETY: `buf` is writable for its full length.
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        let len = usize::try_from(len).unwrap_or(0);
        if buf[..len] == search.title[..] {
            search.found = hwnd.0 as isize;
            return false.into();
        }
        TRUE
    }

    let mut search = Search {
        title: title.encode_utf16().collect(),
        found: 0,
    };
    // SAFETY: the callback only dereferences `lparam` as the `Search` above.
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(visit),
            LPARAM(std::ptr::from_mut(&mut search) as isize),
        );
    }
    (search.found != 0).then_some(search.found)
}

/// Asks Windows 11 to round the corners of our undecorated window (ignored elsewhere).
pub fn round_corners(hwnd: isize) {
    let preference = DWMWCP_ROUND;
    // SAFETY: `hwnd` is our window and the attribute value is a live local of the right size.
    unsafe {
        let _ = DwmSetWindowAttribute(
            HWND(hwnd as *mut _),
            DWMWA_WINDOW_CORNER_PREFERENCE,
            std::ptr::from_ref(&preference).cast(),
            u32::try_from(size_of_val(&preference)).unwrap_or(0),
        );
    }
}

/// Registers the toggle hotkey on a dedicated thread and toggles the window on every press.
/// `RegisterHotKey` is fine here (unlike ferro-link) because the combination is dedicated.
pub fn spawn_hotkey_thread(
    control: Arc<WindowControl>,
    hotkey: Hotkey,
    label: String,
) -> Result<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    std::thread::Builder::new()
        .name("search-hotkey".into())
        .spawn(move || {
            // SAFETY: thread-level hotkey (no window); messages arrive in this thread's queue.
            let registered =
                unsafe { RegisterHotKey(None, 1, hotkey.modifiers | MOD_NOREPEAT, hotkey.vk) }
                    .with_context(|| format!("{label} is already used by another program"));
            let ok = registered.is_ok();
            let _ = ready_tx.send(registered);
            if !ok {
                return;
            }
            let mut msg = MSG::default();
            // SAFETY: `msg` is a valid out-pointer; 0 means WM_QUIT, -1 an error.
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                if msg.message == WM_HOTKEY {
                    control.toggle();
                }
            }
        })
        .context("cannot spawn the hotkey thread")?;
    ready_rx.recv().context("hotkey thread exited")?
}
