//! Small file-type icons from the shell, cached per extension (SPEC §4).
//!
//! `SHGFI_USEFILEATTRIBUTES` asks for the icon of a file *type*, so the file is never
//! touched and the call is cheap enough for the UI thread.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    GetDIBits, GetObjectW, HBITMAP, HDC,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
};
use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, HICON, ICONINFO};
use windows::core::{Owned, PCWSTR};

/// Icon textures keyed by lowercase extension (`.pdf`), `<dir>` or `<file>`.
#[derive(Default)]
pub struct IconCache {
    textures: HashMap<String, Option<TextureHandle>>,
}

impl IconCache {
    pub fn get(&mut self, ctx: &egui::Context, path: &str, is_dir: bool) -> Option<TextureHandle> {
        let key = cache_key(path, is_dir);
        self.textures
            .entry(key)
            .or_insert_with_key(|key| {
                let image = load(key, is_dir);
                if image.is_none() {
                    tracing::debug!(key, "no icon");
                }
                image.map(|image| {
                    ctx.load_texture(format!("icon{key}"), image, TextureOptions::LINEAR)
                })
            })
            .clone()
    }
}

fn cache_key(path: &str, is_dir: bool) -> String {
    if is_dir {
        return "<dir>".into();
    }
    let name = path.rsplit('\\').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            format!(".{}", ext.to_lowercase())
        }
        _ => "<file>".into(),
    }
}

fn load(key: &str, is_dir: bool) -> Option<ColorImage> {
    let (name, attributes) = if is_dir {
        ("folder".to_owned(), FILE_ATTRIBUTE_DIRECTORY)
    } else if key.starts_with('.') {
        (format!("file{key}"), FILE_ATTRIBUTE_NORMAL)
    } else {
        ("file".to_owned(), FILE_ATTRIBUTE_NORMAL)
    };
    let name: Vec<u16> = OsStr::new(&name).encode_wide().chain([0]).collect();
    let mut info = SHFILEINFOW::default();
    // SAFETY: `name` is NUL-terminated and `info` is a valid out-structure of the given size.
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(name.as_ptr()),
            attributes,
            Some(&mut info),
            u32::try_from(size_of::<SHFILEINFOW>()).ok()?,
            SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    // SAFETY: SHGetFileInfoW returned an icon that the caller must destroy; Owned does so.
    let icon = unsafe { Owned::new(info.hIcon) };
    icon_to_image(*icon)
}

/// Memory DC deleted on drop.
struct MemoryDc(HDC);

impl Drop for MemoryDc {
    fn drop(&mut self) {
        // SAFETY: created by CreateCompatibleDC and deleted once.
        let _ = unsafe { DeleteDC(self.0) };
    }
}

/// Reads a bitmap as top-down 32-bit BGRA rows.
fn bitmap_bits(dc: &MemoryDc, bitmap: HBITMAP, width: i32, height: i32) -> Option<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).ok()?,
            biWidth: width,
            biHeight: -height, // negative: top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = vec![0u8; usize::try_from(width * height * 4).ok()?];
    // SAFETY: `bits` holds width*height 32-bit pixels, matching the header we pass.
    let lines = unsafe {
        GetDIBits(
            dc.0,
            bitmap,
            0,
            u32::try_from(height).ok()?,
            Some(bits.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    (lines == height).then_some(bits)
}

fn icon_to_image(icon: HICON) -> Option<ColorImage> {
    let mut info = ICONINFO::default();
    // SAFETY: `icon` is valid; on success we own both bitmaps (deleted by Owned below).
    unsafe { GetIconInfo(icon, &mut info) }.ok()?;
    // SAFETY: GetIconInfo hands ownership of these bitmaps to the caller.
    let (color, mask) = unsafe { (Owned::new(info.hbmColor), Owned::new(info.hbmMask)) };
    if color.is_invalid() {
        return None; // monochrome icon; not worth supporting
    }

    let mut bitmap = BITMAP::default();
    // SAFETY: `bitmap` is a valid out-structure of the given size.
    let got = unsafe {
        GetObjectW(
            (*color).into(),
            i32::try_from(size_of::<BITMAP>()).ok()?,
            Some(std::ptr::from_mut(&mut bitmap).cast()),
        )
    };
    if got == 0 {
        return None;
    }
    let (width, height) = (bitmap.bmWidth, bitmap.bmHeight);

    // SAFETY: a memory DC compatible with the screen; deleted by MemoryDc.
    let dc = MemoryDc(unsafe { CreateCompatibleDC(None) });
    let mut pixels = bitmap_bits(&dc, *color, width, height)?;
    // Icons without an alpha channel carry transparency in the AND mask instead.
    if pixels.chunks_exact(4).all(|p| p[3] == 0) {
        let mask = bitmap_bits(&dc, *mask, width, height)?;
        for (pixel, m) in pixels.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
            pixel[3] = if m[0] == 0 { 255 } else { 0 };
        }
    }
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2); // BGRA -> RGBA
    }
    let size = [usize::try_from(width).ok()?, usize::try_from(height).ok()?];
    Some(ColorImage::from_rgba_unmultiplied(size, &pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(cache_key(r"C:\a\Report.PDF", false), ".pdf");
        assert_eq!(cache_key(r"C:\a\Makefile", false), "<file>");
        assert_eq!(cache_key(r"C:\a\.gitignore", false), "<file>");
        assert_eq!(cache_key(r"C:\a\src", true), "<dir>");
    }

    #[test]
    fn loads_shell_icons() {
        for (key, dir) in [(".txt", false), ("<dir>", true), ("<file>", false)] {
            let image = load(key, dir).unwrap_or_else(|| panic!("no icon for {key}"));
            assert!(image.size[0] >= 16 && image.size[0] == image.size[1]);
            assert!(
                image.pixels.iter().any(|p| p.a() > 0),
                "{key} icon is fully transparent"
            );
        }
    }
}
