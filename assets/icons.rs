// Application icons, drawn in code so no image files or image crates are needed. Shared by
// the build scripts (which turn them into .ico resources) and the apps (window and tray
// icons) through `include!`; therefore plain items only, no inner attributes.
//
// Each icon is a rounded square with a vertical gradient and a white symbol:
// storage = treemap tiles (blue), search = magnifier (purple), link = chain (green).

/// Which application's icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum IconKind {
    Storage,
    Search,
    Link,
}

/// Sizes stored in the .ico files (Windows picks per DPI and view).
#[allow(dead_code)]
pub const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

/// Gradient colours (top, bottom) as RGB.
fn palette(kind: IconKind) -> ([f32; 3], [f32; 3]) {
    match kind {
        IconKind::Storage => ([0x3B as f32, 0x8E as f32, 0xF5 as f32], [0x1A as f32, 0x5B as f32, 0xC4 as f32]),
        IconKind::Search => ([0x8E as f32, 0x5C as f32, 0xF7 as f32], [0x5B as f32, 0x34 as f32, 0xC2 as f32]),
        IconKind::Link => ([0x2E as f32, 0xB8 as f32, 0x7A as f32], [0x14 as f32, 0x86 as f32, 0x54 as f32]),
    }
}

/// Signed distance to a rounded rectangle given by centre, half size and corner radius.
fn rounded_rect(p: (f32, f32), centre: (f32, f32), half: (f32, f32), radius: f32) -> f32 {
    let qx = (p.0 - centre.0).abs() - half.0 + radius;
    let qy = (p.1 - centre.1).abs() - half.1 + radius;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

/// Signed distance to a rectangle from (x0, y0) to (x1, y1) with rounded corners.
fn tile(p: (f32, f32), x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> f32 {
    rounded_rect(p, ((x0 + x1) / 2.0, (y0 + y1) / 2.0), ((x1 - x0) / 2.0, (y1 - y0) / 2.0), radius)
}

/// Distance from `p` to the segment a-b.
fn segment(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let (apx, apy) = (p.0 - a.0, p.1 - a.1);
    let t = ((apx * abx + apy * aby) / (abx * abx + aby * aby)).clamp(0.0, 1.0);
    let (dx, dy) = (apx - t * abx, apy - t * aby);
    (dx * dx + dy * dy).sqrt()
}

/// Signed distance to the white symbol (negative inside) in unit coordinates.
fn symbol(kind: IconKind, p: (f32, f32)) -> f32 {
    match kind {
        IconKind::Storage => {
            let r = 0.035;
            tile(p, 0.22, 0.22, 0.50, 0.78, r)
                .min(tile(p, 0.55, 0.22, 0.78, 0.47, r))
                .min(tile(p, 0.55, 0.52, 0.65, 0.78, r))
                .min(tile(p, 0.70, 0.52, 0.78, 0.78, r))
        }
        IconKind::Search => {
            let centre = (0.43, 0.43);
            let d = ((p.0 - centre.0).powi(2) + (p.1 - centre.1).powi(2)).sqrt();
            let ring = (d - 0.19).abs() - 0.055;
            let handle = segment(p, (0.58, 0.58), (0.76, 0.76)) - 0.065;
            ring.min(handle)
        }
        IconKind::Link => {
            // Two interlocking chain links (stadium outlines) along the diagonal.
            let first = (segment(p, (0.30, 0.70), (0.46, 0.54)) - 0.125).abs() - 0.05;
            let second = (segment(p, (0.54, 0.46), (0.70, 0.30)) - 0.125).abs() - 0.05;
            first.min(second)
        }
    }
}

/// Renders `size`×`size` top-down RGBA pixels with straight (non-premultiplied) alpha.
#[allow(dead_code)]
pub fn render_icon(kind: IconKind, size: u32) -> Vec<u8> {
    let (top, bottom) = palette(kind);
    // More samples for small sizes, where every pixel edge shows.
    let samples: u32 = if size <= 32 { 6 } else if size <= 64 { 4 } else { 2 };
    let n = size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut shape, mut white) = (0.0f32, 0.0f32);
            for sy in 0..samples {
                for sx in 0..samples {
                    let p = (
                        (x as f32 + (sx as f32 + 0.5) / samples as f32) / n,
                        (y as f32 + (sy as f32 + 0.5) / samples as f32) / n,
                    );
                    if rounded_rect(p, (0.5, 0.5), (0.46, 0.46), 0.2) <= 0.0 {
                        shape += 1.0;
                        if symbol(kind, p) <= 0.0 {
                            white += 1.0;
                        }
                    }
                }
            }
            let total = (samples * samples) as f32;
            let alpha = shape / total;
            let mix = if shape > 0.0 { white / shape } else { 0.0 };
            let t = (y as f32 + 0.5) / n;
            for c in 0..3 {
                let base = top[c] + (bottom[c] - top[c]) * t;
                let value = base + (255.0 - base) * mix;
                out.push(value.round().clamp(0.0, 255.0) as u8);
            }
            out.push((alpha * 255.0).round() as u8);
        }
    }
    out
}

/// A complete .ico file with 32-bit images at `ICO_SIZES` (BMP entries, no PNG encoder
/// needed; Windows accepts BMP up to 256 px).
#[allow(dead_code)]
pub fn ico_file(kind: IconKind) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = ICO_SIZES
        .iter()
        .map(|&size| {
            let rgba = render_icon(kind, size);
            let mask_stride = size.div_ceil(32) * 4;
            let mut image = Vec::new();
            // BITMAPINFOHEADER; the height counts the colour and mask bitmaps together.
            for value in [40u32, size, size * 2] {
                image.extend_from_slice(&value.to_le_bytes());
            }
            image.extend_from_slice(&1u16.to_le_bytes()); // planes
            image.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
            for value in [0u32, size * size * 4 + mask_stride * size, 0, 0, 0, 0] {
                image.extend_from_slice(&value.to_le_bytes());
            }
            // Colour bitmap: bottom-up BGRA.
            for y in (0..size).rev() {
                for x in 0..size {
                    let i = ((y * size + x) * 4) as usize;
                    image.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
                }
            }
            // AND mask: all transparent bits off, the alpha channel decides.
            image.extend(std::iter::repeat_n(0u8, (mask_stride * size) as usize));
            (size, image)
        })
        .collect();

    let mut file = Vec::new();
    file.extend_from_slice(&0u16.to_le_bytes()); // reserved
    file.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    file.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, image) in &images {
        let side = if *size >= 256 { 0u8 } else { *size as u8 };
        file.extend_from_slice(&[side, side, 0, 0]);
        file.extend_from_slice(&1u16.to_le_bytes()); // planes
        file.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        file.extend_from_slice(&(image.len() as u32).to_le_bytes());
        file.extend_from_slice(&offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for (_, image) in &images {
        file.extend_from_slice(image);
    }
    file
}
