//! Tray icon drawn at runtime: a white chain link on a blue rounded square.
//! Drawing it avoids a resource compiler and scales to the current small-icon size.

use anyhow::{Context, Result};
use windows::Win32::UI::WindowsAndMessaging::{CreateIcon, GetSystemMetrics, HICON, SM_CXSMICON};
use windows::core::Owned;

const BACKGROUND: [u8; 3] = [0x2F, 0x6F, 0xEB]; // RGB
const FOREGROUND: [u8; 3] = [0xFF, 0xFF, 0xFF];

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

/// Renders `size`×`size` top-down BGRA pixels (straight alpha) with simple anti-aliasing.
fn render(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size * size * 4);
    #[allow(clippy::cast_precision_loss)]
    let scale = size as f32;
    let aa = 1.0 / scale;
    let diagonal = (
        std::f32::consts::FRAC_1_SQRT_2,
        -std::f32::consts::FRAC_1_SQRT_2,
    );
    let links = [(0.38f32, 0.62f32), (0.62, 0.38)];

    for y in 0..size {
        for x in 0..size {
            #[allow(clippy::cast_precision_loss)]
            let p = ((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale);

            let background = coverage(rounded_square(p, 0.5, 0.2), aa);
            let ring = links
                .iter()
                .map(|&center| {
                    let a = (center.0 - diagonal.0 * 0.1, center.1 - diagonal.1 * 0.1);
                    let b = (center.0 + diagonal.0 * 0.1, center.1 + diagonal.1 * 0.1);
                    let d = segment_distance(p, a, b) - 0.13;
                    coverage(d.abs() - 0.045, aa)
                })
                .fold(0.0f32, f32::max);

            let mix = |bg: u8, fg: u8| {
                let v = f32::from(bg) * (1.0 - ring) + f32::from(fg) * ring;
                // In 0..=255 by construction.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let v = v.round() as u8;
                v
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let alpha = (background * 255.0).round() as u8;
            out.extend_from_slice(&[
                mix(BACKGROUND[2], FOREGROUND[2]),
                mix(BACKGROUND[1], FOREGROUND[1]),
                mix(BACKGROUND[0], FOREGROUND[0]),
                alpha,
            ]);
        }
    }
    out
}

/// 1 inside, 0 outside, linear ramp over one pixel around the edge.
fn coverage(signed_distance: f32, aa: f32) -> f32 {
    (0.5 - signed_distance / aa).clamp(0.0, 1.0)
}

fn rounded_square(p: (f32, f32), half: f32, radius: f32) -> f32 {
    let qx = (p.0 - 0.5).abs() - (half - radius);
    let qy = (p.1 - 0.5).abs() - (half - radius);
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    outside + qx.max(qy).min(0.0) - radius
}

fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (pax, pay) = (p.0 - a.0, p.1 - a.1);
    let (bax, bay) = (b.0 - a.0, b.1 - a.1);
    let t = ((pax * bax + pay * bay) / (bax * bax + bay * bay)).clamp(0.0, 1.0);
    (pax - bax * t).hypot(pay - bay * t)
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
