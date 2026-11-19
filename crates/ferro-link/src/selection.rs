//! Reads the selection and current folder of an Explorer window or the desktop (SPEC §1.3).
//!
//! Must be called on a thread with an initialised COM apartment.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, IDispatch, IServiceProvider};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, IFolderView2, IShellBrowser, IShellItem, IShellWindows, IWebBrowserApp,
    SID_STopLevelBrowser, SIGDN_FILESYSPATH, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::FindWindowExW;
use windows::core::{Interface, w};

use crate::hook::DESKTOP_CLASSES;
use crate::wide::{CoTaskMemWide, window_class_is};

/// What the user sees in one Explorer tab (or on the desktop).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExplorerView {
    /// Folder shown in the view; `None` for virtual locations (This PC, Recycle Bin, …).
    pub folder: Option<PathBuf>,
    /// Selected items that have a filesystem path.
    pub selection: Vec<PathBuf>,
    /// Selected items without a filesystem path (zip contents, phones, …).
    pub virtual_items: usize,
}

/// Reads the view of `window`, a top-level Explorer window (`CabinetWClass`) or the desktop.
pub fn read(window: HWND) -> Result<ExplorerView> {
    // SAFETY: COM is initialised on this thread (module contract).
    let shell_windows: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }
        .context("cannot create ShellWindows")?;
    let browser = if window_class_is(window, &DESKTOP_CLASSES) {
        desktop_browser(&shell_windows)?
    } else {
        explorer_browser(&shell_windows, window)?
    };
    read_view(&browser)
}

/// Finds the browser of the active tab inside `window`.
///
/// On Windows 11 every tab is a separate ShellWindows entry sharing the same top-level
/// HWND. The active tab is the first `ShellTabWindowClass` child in z-order, which is
/// compared against each entry's `IShellBrowser::GetWindow`.
fn explorer_browser(shell_windows: &IShellWindows, window: HWND) -> Result<IShellBrowser> {
    // SAFETY: plain window query; a missing child yields an error that we treat as "no tabs".
    let active_tab =
        unsafe { FindWindowExW(Some(window), None, w!("ShellTabWindowClass"), None) }.ok();

    let mut candidates = Vec::new();
    for entry in shell_windows_entries(shell_windows)? {
        let Ok(app) = entry.cast::<IWebBrowserApp>() else {
            continue;
        };
        // SAFETY: `app` is a valid interface pointer.
        let Ok(handle) = (unsafe { app.HWND() }) else {
            continue;
        };
        if handle.0 != window.0 as isize {
            continue;
        }
        if let Ok(browser) = top_level_browser(&entry) {
            candidates.push(browser);
        }
    }

    if candidates.len() > 1
        && let Some(tab) = active_tab
    {
        for browser in candidates {
            // SAFETY: `browser` is a valid interface pointer.
            if unsafe { browser.GetWindow() }.is_ok_and(|hwnd| hwnd == tab) {
                return Ok(browser);
            }
        }
        bail!("cannot determine the active Explorer tab");
    }
    candidates
        .into_iter()
        .next()
        .context("the Explorer window is not registered in ShellWindows")
}

fn desktop_browser(shell_windows: &IShellWindows) -> Result<IShellBrowser> {
    let location = VARIANT::from(i32::try_from(CSIDL_DESKTOP)?);
    let root = VARIANT::default();
    let mut hwnd = 0i32;
    // SAFETY: all pointers reference live locals for the duration of the call.
    let dispatch = unsafe {
        shell_windows.FindWindowSW(&location, &root, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)
    }
    .context("cannot find the desktop in ShellWindows")?;
    top_level_browser(&dispatch)
}

fn shell_windows_entries(shell_windows: &IShellWindows) -> Result<Vec<IDispatch>> {
    // SAFETY: `shell_windows` is a valid interface pointer.
    let count = unsafe { shell_windows.Count() }.context("ShellWindows.Count failed")?;
    Ok((0..count)
        // SAFETY: `index` is in range; a window closing meanwhile only yields an error.
        .filter_map(|index| unsafe { shell_windows.Item(&VARIANT::from(index)) }.ok())
        .collect())
}

fn top_level_browser(entry: &IDispatch) -> Result<IShellBrowser> {
    let provider: IServiceProvider = entry.cast().context("entry has no IServiceProvider")?;
    // SAFETY: `provider` is valid and the GUID is a static constant.
    unsafe { provider.QueryService(&SID_STopLevelBrowser) }.context("no top-level browser")
}

fn read_view(browser: &IShellBrowser) -> Result<ExplorerView> {
    // SAFETY: `browser` is a valid interface pointer.
    let view = unsafe { browser.QueryActiveShellView() }.context("no active shell view")?;
    let folder_view: IFolderView2 = view.cast().context("view is not an IFolderView2")?;

    // SAFETY: `folder_view` is a valid interface pointer.
    let folder = unsafe { folder_view.GetFolder::<IShellItem>() }
        .ok()
        .and_then(|item| filesystem_path(&item));

    let mut result = ExplorerView {
        folder,
        ..ExplorerView::default()
    };
    // GetSelection fails when nothing is selected; that is an empty selection, not an error.
    // SAFETY: `folder_view` is a valid interface pointer.
    let Ok(items) = (unsafe { folder_view.GetSelection(false) }) else {
        return Ok(result);
    };
    // SAFETY: `items` is a valid interface pointer.
    let count = unsafe { items.GetCount() }.context("IShellItemArray.GetCount failed")?;
    for index in 0..count {
        // SAFETY: `index` < `count`.
        let item = unsafe { items.GetItemAt(index) }.context("IShellItemArray.GetItemAt failed")?;
        match filesystem_path(&item) {
            Some(path) => result.selection.push(path),
            None => result.virtual_items += 1,
        }
    }
    Ok(result)
}

fn filesystem_path(item: &IShellItem) -> Option<PathBuf> {
    // SAFETY: `item` is valid; the returned string is CoTaskMem-allocated and owned by us.
    let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
    // SAFETY: GetDisplayName returns a CoTaskMemAlloc'd, NUL-terminated string.
    let name = unsafe { CoTaskMemWide::new(name) };
    let path = name.to_path();
    (!path.as_os_str().is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

    use super::*;

    /// Opens a real Explorer window with `explorer /select` on a temp file and reads it back.
    /// Ignored by default because it opens a window on the desktop.
    #[test]
    #[ignore = "opens an Explorer window"]
    fn reads_selection_of_explorer_window() {
        let dir = std::env::temp_dir().join(format!("ferrofs-selection-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("picked.txt");
        std::fs::write(&file, b"x").unwrap();

        // SAFETY: test thread, balanced by CoUninitialize below.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
        Command::new("explorer.exe")
            .arg(format!("/select,{}", file.display()))
            .status() // explorer.exe hands off to the shell and exits; its code is meaningless
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        let view = loop {
            if let Some(view) = find_view_of(&dir) {
                break view;
            }
            assert!(Instant::now() < deadline, "Explorer window did not appear");
            std::thread::sleep(Duration::from_millis(250));
        };

        assert_eq!(view.virtual_items, 0);
        assert_eq!(view.selection.len(), 1, "{view:?}");
        assert_eq!(view.selection[0].file_name(), file.file_name());
        // SAFETY: matches CoInitializeEx above.
        unsafe { CoUninitialize() };
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Reads every Explorer window and returns the view showing `dir`.
    fn find_view_of(dir: &std::path::Path) -> Option<ExplorerView> {
        // SAFETY: COM is initialised by the calling test.
        let shell_windows: IShellWindows =
            unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }.ok()?;
        let wanted = dir.file_name()?;
        shell_windows_entries(&shell_windows)
            .ok()?
            .iter()
            .filter_map(|entry| top_level_browser(entry).ok())
            .filter_map(|browser| read_view(&browser).ok())
            .find(|view| {
                view.folder.as_deref().and_then(|f| f.file_name()) == Some(wanted)
                    && !view.selection.is_empty()
            })
    }
}
