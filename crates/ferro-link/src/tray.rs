//! Tray icon, context menu and balloon notifications (SPEC §1.4 feedback).
//!
//! Implemented directly on `Shell_NotifyIconW` because balloon notifications
//! (`NIF_INFO`) are required and the `tray-icon` crate does not expose them.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_WARNING,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, IDI_APPLICATION, LoadIconW, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP,
    WM_CONTEXTMENU, WM_NULL, WM_RBUTTONUP, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::{Owned, w};

use crate::wide::copy_into;

const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const MENU_QUIT: usize = 1;

/// Message id of "TaskbarCreated" (sent when Explorer restarts); 0 until registered.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Owns the hidden window and the tray icon. Must live on the thread that runs [`Tray::run`].
#[derive(Debug)]
pub struct Tray {
    hwnd: HWND,
}

impl Tray {
    pub fn new() -> Result<Self> {
        // SAFETY: passing None returns the handle of the current executable.
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }
            .context("GetModuleHandleW failed")?
            .into();
        let class_name = w!("ferrofs.link.tray");
        let class = WNDCLASSEXW {
            cbSize: u32::try_from(size_of::<WNDCLASSEXW>())?,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        // SAFETY: `class` is fully initialised and its strings are static.
        if unsafe { RegisterClassExW(&class) } == 0 {
            return Err(windows::core::Error::from_thread()).context("RegisterClassExW failed");
        }

        // SAFETY: the "TaskbarCreated" literal is static and NUL-terminated.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        TASKBAR_CREATED.store(taskbar_created, Ordering::Relaxed);

        // A real (never shown) top-level window rather than a message-only one,
        // because message-only windows do not receive "TaskbarCreated".
        // SAFETY: the class was registered above; all pointers are static or None.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                w!("ferro-link"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .context("CreateWindowExW failed")?;

        let tray = Self { hwnd };
        add_icon(hwnd)?;
        Ok(tray)
    }

    /// A thread-safe handle for showing notifications from other threads.
    pub fn notifier(&self) -> Notifier {
        Notifier {
            hwnd: self.hwnd.0 as isize,
        }
    }

    /// Runs the message loop until "Quit" is chosen.
    pub fn run(self) -> Result<()> {
        let mut msg = MSG::default();
        loop {
            // SAFETY: `msg` is a valid out-pointer.
            let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
            match ret {
                0 => return Ok(()),
                -1 => {
                    return Err(windows::core::Error::from_thread()).context("GetMessageW failed");
                }
                _ => {
                    // SAFETY: `msg` was filled by GetMessageW.
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let data = base_data(self.hwnd);
        // SAFETY: `data` identifies the icon added in `new`.
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        // SAFETY: the window was created by this thread in `new` and is destroyed once.
        if let Err(err) = unsafe { DestroyWindow(self.hwnd) } {
            tracing::warn!("DestroyWindow failed: {err}");
        }
    }
}

/// Severity of a tray notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    #[allow(dead_code, reason = "used for link failures in the next step")]
    Warning,
    Error,
}

/// Shows balloon/toast notifications on the tray icon. `Send`, cheap to copy.
#[derive(Debug, Clone, Copy)]
pub struct Notifier {
    /// HWND stored as an integer so the type is `Send`.
    hwnd: isize,
}

impl Notifier {
    pub fn notify(&self, level: Level, title: &str, body: &str) {
        let mut data = base_data(HWND(self.hwnd as *mut c_void));
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = match level {
            Level::Info => NIIF_INFO,
            Level::Warning => NIIF_WARNING,
            Level::Error => NIIF_ERROR,
        };
        copy_into(&mut data.szInfoTitle, title);
        copy_into(&mut data.szInfo, body);
        // SAFETY: `data` is fully initialised and identifies our icon; Shell_NotifyIconW
        // may be called from any thread.
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
            tracing::warn!(title, body, "cannot show tray notification");
        }
    }
}

fn base_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
        hWnd: hwnd,
        uID: TRAY_ID,
        ..Default::default()
    }
}

fn add_icon(hwnd: HWND) -> Result<()> {
    let mut data = base_data(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = WM_TRAY;
    // TODO(icon): ship a proper ferrofs icon as a resource.
    // SAFETY: loading a predefined system icon; shared icons must not be destroyed.
    data.hIcon = unsafe { LoadIconW(None, IDI_APPLICATION) }.context("LoadIconW failed")?;
    copy_into(&mut data.szTip, "ferro-link  (Ctrl+B pick, Ctrl+G drop)");

    // SAFETY: `data` is fully initialised.
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        bail!("Shell_NotifyIconW(NIM_ADD) failed");
    }
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    // SAFETY: `data` is fully initialised.
    if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
        bail!("Shell_NotifyIconW(NIM_SETVERSION) failed");
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_TRAY {
        // With NOTIFYICON_VERSION_4 the low word of lparam carries the event.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let event = u32::from(lparam.0 as u16);
        if event == WM_CONTEXTMENU || event == WM_RBUTTONUP {
            show_menu(hwnd);
        }
        return LRESULT(0);
    }
    let taskbar_created = TASKBAR_CREATED.load(Ordering::Relaxed);
    if taskbar_created != 0 && msg == taskbar_created {
        tracing::info!("taskbar recreated; re-adding tray icon");
        if let Err(err) = add_icon(hwnd) {
            tracing::error!("cannot re-add tray icon: {err:#}");
        }
        return LRESULT(0);
    }
    // SAFETY: default handling with the unmodified arguments.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn show_menu(hwnd: HWND) {
    // SAFETY: no preconditions; the menu is destroyed by Owned on drop.
    let menu = match unsafe { CreatePopupMenu() } {
        // SAFETY: the menu was just created and is owned only here.
        Ok(menu) => unsafe { Owned::new(menu) },
        Err(err) => {
            tracing::error!("CreatePopupMenu failed: {err}");
            return;
        }
    };
    // SAFETY: `menu` is valid; the label is a static NUL-terminated literal.
    if let Err(err) = unsafe { AppendMenuW(*menu, MF_STRING, MENU_QUIT, w!("Quit")) } {
        tracing::error!("AppendMenuW failed: {err}");
        return;
    }

    let mut cursor = POINT::default();
    // SAFETY: `cursor` is a valid out-pointer.
    let _ = unsafe { GetCursorPos(&mut cursor) };
    // The window must be foreground or the menu will not close when clicking elsewhere.
    // SAFETY: `hwnd` is our window and `menu` is valid for the duration of the call.
    let chosen = unsafe {
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            *menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            cursor.x,
            cursor.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        chosen
    };

    if usize::try_from(chosen.0).ok() == Some(MENU_QUIT) {
        tracing::info!("quit requested from tray menu");
        // SAFETY: no preconditions; ends this thread's message loop.
        unsafe { PostQuitMessage(0) };
    }
}
