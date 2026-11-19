//! Tray icon, menus and balloon notifications (SPEC §1.4 feedback).
//!
//! Implemented directly on `Shell_NotifyIconW` because balloon notifications
//! (`NIF_INFO`) are required and the `tray-icon` crate does not expose them.
//! All menus run on the tray thread, which owns the hidden window.

use std::ffi::{OsStr, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_WARNING,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, HICON,
    MENU_ITEM_FLAGS, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SW_SHOWNORMAL, SetForegroundWindow, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP,
    WM_CLOSE, WM_CONTEXTMENU, WM_NULL, WM_RBUTTONUP, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::{Owned, PCWSTR, w};

use crate::wide::{copy_into, to_wide_nul};
use crate::worker::Command;
use crate::{autostart, config, paths};

const WM_TRAY: u32 = WM_APP + 1;
/// Posted by [`TrayHandle::choose_link_type`]; `lparam` owns a `Box<ChooseRequest>`.
const WM_CHOOSE_LINK_TYPE: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;

/// Message id of "TaskbarCreated" (sent when Explorer restarts); 0 until registered.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// Raw HICON of the tray icon (owned by [`Tray`]), needed to re-add it after Explorer restarts.
static ICON: AtomicIsize = AtomicIsize::new(0);
/// Channel to the worker, used by menu commands such as "Reload config".
static WORKER: OnceLock<Sender<Command>> = OnceLock::new();

/// Owns the hidden window and the tray icon. Must live on the thread that runs [`Tray::run`].
#[derive(Debug)]
pub struct Tray {
    hwnd: HWND,
    _icon: Owned<HICON>,
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

        let icon = crate::icon::create()?;
        ICON.store(icon.0 as isize, Ordering::Relaxed);
        let tray = Self { hwnd, _icon: icon };
        add_icon(hwnd)?;
        Ok(tray)
    }

    /// A thread-safe handle for notifications and menus requested from other threads.
    pub fn handle(&self) -> TrayHandle {
        TrayHandle {
            hwnd: self.hwnd.0 as isize,
        }
    }

    /// Lets tray menu commands reach the worker. Call once after spawning it.
    pub fn connect_worker(&self, sender: Sender<Command>) {
        if WORKER.set(sender).is_err() {
            tracing::warn!("worker already connected to the tray");
        }
    }

    /// Runs the message loop until "Quit" is chosen or `WM_CLOSE` is received.
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
        ICON.store(0, Ordering::Relaxed);
    }
}

/// Severity of a tray notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

/// Link type picked in the Shift+drop menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkChoice {
    /// The modes from the config file.
    Configured,
    /// Symlinks for files and folders.
    Symlinks,
    /// Hard links for files, junctions for folders.
    HardlinksAndJunctions,
}

struct ChooseRequest {
    configured_label: String,
    has_files: bool,
    has_dirs: bool,
    reply: SyncSender<Option<LinkChoice>>,
}

/// Talks to the tray thread from anywhere. `Send`, cheap to copy.
#[derive(Debug, Clone, Copy)]
pub struct TrayHandle {
    /// HWND stored as an integer so the type is `Send`.
    hwnd: isize,
}

impl TrayHandle {
    fn hwnd(self) -> HWND {
        HWND(self.hwnd as *mut c_void)
    }

    pub fn notify(self, level: Level, title: &str, body: &str) {
        let mut data = base_data(self.hwnd());
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

    /// Shows the link-type menu at the cursor and blocks until the user picks an entry.
    /// Returns `None` if the menu was dismissed.
    pub fn choose_link_type(
        self,
        configured_label: String,
        has_files: bool,
        has_dirs: bool,
    ) -> Option<LinkChoice> {
        let (reply, answer) = mpsc::sync_channel(1);
        let request = Box::into_raw(Box::new(ChooseRequest {
            configured_label,
            has_files,
            has_dirs,
            reply,
        }));
        // SAFETY: on success ownership of `request` passes to the tray thread (see window_proc).
        let posted = unsafe {
            PostMessageW(
                Some(self.hwnd()),
                WM_CHOOSE_LINK_TYPE,
                WPARAM(0),
                LPARAM(request as isize),
            )
        };
        if let Err(err) = posted {
            tracing::error!("cannot show the link type menu: {err}");
            // SAFETY: the message was not posted, so we still own `request`.
            drop(unsafe { Box::from_raw(request) });
            return None;
        }
        answer.recv_timeout(Duration::from_secs(300)).ok().flatten()
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
    data.hIcon = HICON(ICON.load(Ordering::Relaxed) as *mut c_void);
    copy_into(&mut data.szTip, "ferro-link: pick and drop links");

    // SAFETY: `data` is fully initialised; the icon stays alive while the Tray exists.
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
    match msg {
        WM_TRAY => {
            // With NOTIFYICON_VERSION_4 the low word of lparam carries the event.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let event = u32::from(lparam.0 as u16);
            if event == WM_CONTEXTMENU || event == WM_RBUTTONUP {
                show_tray_menu(hwnd);
            }
            return LRESULT(0);
        }
        WM_CHOOSE_LINK_TYPE => {
            // SAFETY: `lparam` is the Box<ChooseRequest> posted by `choose_link_type`, whose
            // ownership was handed over with the message.
            let request = unsafe { Box::from_raw(lparam.0 as *mut ChooseRequest) };
            let choice = show_link_type_menu(hwnd, &request);
            let _ = request.reply.send(choice);
            return LRESULT(0);
        }
        WM_CLOSE => {
            // Lets installers and scripts close the app gracefully.
            tracing::info!("WM_CLOSE received");
            // SAFETY: no preconditions; ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        _ => {}
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

const TRAY_OPEN_CONFIG: usize = 1;
const TRAY_RELOAD_CONFIG: usize = 2;
const TRAY_AUTOSTART: usize = 3;
const TRAY_QUIT: usize = 4;

fn show_tray_menu(hwnd: HWND) {
    let autostart_enabled = autostart::is_enabled();
    let items = [
        MenuItem::new(TRAY_OPEN_CONFIG, "Open config file"),
        MenuItem::new(TRAY_RELOAD_CONFIG, "Reload config"),
        MenuItem::separator(),
        MenuItem::new(TRAY_AUTOSTART, "Start with Windows").checked(autostart_enabled),
        MenuItem::separator(),
        MenuItem::new(TRAY_QUIT, "Quit"),
    ];
    match track_menu(hwnd, &items, false) {
        Some(TRAY_OPEN_CONFIG) => open_config(hwnd),
        Some(TRAY_RELOAD_CONFIG) => {
            if let Some(worker) = WORKER.get()
                && worker.send(Command::ReloadConfig).is_err()
            {
                tracing::error!("worker is gone; cannot reload config");
            }
        }
        Some(TRAY_AUTOSTART) => {
            if let Err(err) = autostart::set_enabled(!autostart_enabled) {
                tracing::error!("cannot change autostart: {err:#}");
                TrayHandle {
                    hwnd: hwnd.0 as isize,
                }
                .notify(Level::Error, "Start with Windows", &format!("{err:#}"));
            } else {
                tracing::info!(enabled = !autostart_enabled, "autostart changed");
            }
        }
        Some(TRAY_QUIT) => {
            tracing::info!("quit requested from tray menu");
            // SAFETY: no preconditions; ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
        }
        _ => {}
    }
}

const CHOICE_CONFIGURED: usize = 1;
const CHOICE_SYMLINKS: usize = 2;
const CHOICE_HARD: usize = 3;

fn show_link_type_menu(hwnd: HWND, request: &ChooseRequest) -> Option<LinkChoice> {
    let hard_label = match (request.has_files, request.has_dirs) {
        (true, false) => "&Hard links",
        (false, true) => "&Junctions",
        _ => "&Hard links (files) and junctions (folders)",
    };
    let items = [
        MenuItem::new(
            CHOICE_CONFIGURED,
            &format!("&Default ({})", request.configured_label),
        ),
        MenuItem::new(CHOICE_SYMLINKS, "&Symbolic links"),
        MenuItem::new(CHOICE_HARD, hard_label),
    ];
    // Opened from a hotkey over Explorer: hand focus back to it afterwards.
    match track_menu(hwnd, &items, true)? {
        CHOICE_CONFIGURED => Some(LinkChoice::Configured),
        CHOICE_SYMLINKS => Some(LinkChoice::Symlinks),
        CHOICE_HARD => Some(LinkChoice::HardlinksAndJunctions),
        _ => None,
    }
}

/// Opens the config file in its associated editor, falling back to Notepad.
fn open_config(hwnd: HWND) {
    let result = paths::config_file().and_then(|path| {
        if !path.exists() {
            config::load_or_create(&path)?;
        }
        Ok(path)
    });
    let path = match result {
        Ok(path) => path,
        Err(err) => {
            tracing::error!("cannot prepare config file: {err:#}");
            return;
        }
    };
    let file = to_wide_nul(path.as_os_str());
    // SAFETY: all strings are NUL-terminated and outlive the call.
    let code = unsafe {
        ShellExecuteW(
            Some(hwnd),
            w!("open"),
            PCWSTR(file.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // Values <= 32 are errors, e.g. no program associated with .toml.
    if code.0 as isize <= 32 {
        // SAFETY: all strings are NUL-terminated and outlive the call.
        unsafe {
            ShellExecuteW(
                Some(hwnd),
                w!("open"),
                w!("notepad.exe"),
                PCWSTR(file.as_ptr()),
                None,
                SW_SHOWNORMAL,
            )
        };
    }
}

struct MenuItem {
    id: usize,
    label: Vec<u16>,
    flags: MENU_ITEM_FLAGS,
}

impl MenuItem {
    fn new(id: usize, label: &str) -> Self {
        Self {
            id,
            label: to_wide_nul(OsStr::new(label)),
            flags: MF_STRING,
        }
    }

    fn separator() -> Self {
        Self {
            id: 0,
            label: vec![0],
            flags: MF_SEPARATOR,
        }
    }

    fn checked(mut self, checked: bool) -> Self {
        if checked {
            self.flags |= MF_CHECKED;
        }
        self
    }
}

/// Shows a popup menu at the cursor and returns the chosen id (`None` if dismissed).
/// With `restore_focus`, the previously focused window is re-activated afterwards.
fn track_menu(hwnd: HWND, items: &[MenuItem], restore_focus: bool) -> Option<usize> {
    // SAFETY: no preconditions.
    let menu = match unsafe { CreatePopupMenu() } {
        // SAFETY: the menu was just created and is owned only here; Owned destroys it.
        Ok(menu) => unsafe { Owned::new(menu) },
        Err(err) => {
            tracing::error!("CreatePopupMenu failed: {err}");
            return None;
        }
    };
    for item in items {
        let label = PCWSTR(item.label.as_ptr());
        // SAFETY: `menu` is valid; `label` is NUL-terminated and outlives the call.
        let appended = unsafe { AppendMenuW(*menu, item.flags, item.id, label) };
        if let Err(err) = appended {
            tracing::error!("AppendMenuW failed: {err}");
            return None;
        }
    }

    let mut cursor = POINT::default();
    // SAFETY: `cursor` is a valid out-pointer.
    let _ = unsafe { GetCursorPos(&mut cursor) };
    // SAFETY: no preconditions.
    let previous = unsafe { GetForegroundWindow() };
    // The window must be foreground or the menu ignores the keyboard and does not
    // close when clicking elsewhere.
    bring_to_foreground(hwnd);
    // SAFETY: `hwnd` is our window and `menu` is valid for the duration of the call.
    let chosen = unsafe {
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
    if restore_focus && !previous.is_invalid() && previous != hwnd {
        // SAFETY: we are the foreground process now, so activating another window is allowed;
        // a stale handle just fails.
        let _ = unsafe { SetForegroundWindow(previous) };
    }
    usize::try_from(chosen.0).ok().filter(|&id| id != 0)
}

/// `SetForegroundWindow` is refused unless this process owns the last input event. When a
/// menu is opened by a hotkey (whose keystroke we swallowed), temporarily attaching to
/// the foreground thread's input queue makes the request succeed.
fn bring_to_foreground(hwnd: HWND) {
    // SAFETY: plain window/thread queries; the attachment is undone before returning.
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() {
            return;
        }
        let foreground_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let this_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != this_thread
            && AttachThreadInput(this_thread, foreground_thread, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(this_thread, foreground_thread, false);
        }
    }
}
