//! Tray icon drawn at runtime with the shared ferrofs artwork (`assets/icons.rs`): a white
//! chain on a green rounded square. Drawing it scales to the current small-icon size.

use anyhow::{Context, Result};
use windows::Win32::UI::WindowsAndMessaging::{CreateIcon, GetSystemMetrics, HICON, SM_CXSMICON};
use windows::core::Owned;

mod art {
    include!("../../../assets/icons.rs");
}

/// Creates the icon at the system small-icon size (16 px at 100 % scaling).
pub fn create() -> Result<Owned<HICON>> {
    // SAFETY: no preconditions.
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 256);
    let size = usize::try_from(size)?;
    let bgra = render(size);

    // 1-bpp AND mask, rows padded to 16 bits; all zeros so the alpha channel decides.
    let mask_stride = size.div_ceil(16) * 2;
    let mask = vec![0u8; mask_stride * size];
    let side = i32::try_from(size)?;
    // SAFETY: `mask` holds `size` rows of `mask_stride` bytes and `bgra` holds size*size
    // 32-bit pixels, matching the width/height/bit depth passed.
    let icon = unsafe { CreateIcon(None, side, side, 1, 32, mask.as_ptr(), bgra.as_ptr()) }
        .context("CreateIcon failed")?;
    // SAFETY: the icon was just created and is owned only here; Owned destroys it.
    Ok(unsafe { Owned::new(icon) })
}

/// Renders `size`×`size` top-down BGRA pixels (straight alpha).
fn render(size: usize) -> Vec<u8> {
    let side = u32::try_from(size).unwrap_or(16);
    let mut pixels = art::render_icon(art::IconKind::Link, side);
    for px in pixels.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA -> BGRA
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn render_has_opaque_center_and_transparent_corner() {
        let size = 32;
        let px = render(size);
        assert_eq!(px.len(), size * size * 4);
        assert_eq!(px[3], 0, "corner should be transparent");
        let center = (size / 2 * size + size / 2) * 4;
        assert_eq!(px[center + 3], 255, "center should be opaque");
    }
}
