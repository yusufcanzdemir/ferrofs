//! Squarified treemap layout (Bruls, Huizing, van Wijk 2000): items are laid out in rows
//! along the shorter side of the remaining rectangle, and a row grows while that keeps
//! its worst aspect ratio from getting worse.

use eframe::egui::{Rect, pos2};

/// Rectangles for `sizes` (largest first for good results) filling `rect`, with areas
/// proportional to the sizes. Items with size 0 get empty rectangles.
pub fn squarify(sizes: &[f64], rect: Rect) -> Vec<Rect> {
    let mut out = vec![Rect::NOTHING; sizes.len()];
    let total: f64 = sizes.iter().filter(|s| **s > 0.0).sum();
    let area = f64::from(rect.width()) * f64::from(rect.height());
    if total <= 0.0 || area <= 0.0 {
        return out;
    }
    let scale = area / total;
    // Remaining free space as (x, y, w, h) in f64 to avoid drift over many items.
    let (mut x, mut y) = (f64::from(rect.left()), f64::from(rect.top()));
    let (mut w, mut h) = (f64::from(rect.width()), f64::from(rect.height()));
    let items: Vec<usize> = (0..sizes.len()).filter(|&i| sizes[i] > 0.0).collect();
    let mut start = 0;
    while start < items.len() {
        let side = w.min(h);
        if side <= 0.0 {
            break;
        }
        // Grow the row while the worst aspect ratio improves.
        let (mut sum, mut largest, mut smallest) = (0.0, 0.0_f64, f64::INFINITY);
        let mut worst = f64::INFINITY;
        let mut end = start;
        while end < items.len() {
            let s = sizes[items[end]] * scale;
            let next = worst_ratio(sum + s, largest.max(s), smallest.min(s), side);
            if end > start && next > worst {
                break;
            }
            worst = next;
            sum += s;
            largest = largest.max(s);
            smallest = smallest.min(s);
            end += 1;
        }
        // Lay the row out across the short side.
        let thickness = sum / side;
        let mut offset = 0.0;
        for &i in &items[start..end] {
            let length = sizes[i] * scale / thickness;
            out[i] = if w >= h {
                rect_of(x, y + offset, thickness, length)
            } else {
                rect_of(x + offset, y, length, thickness)
            };
            offset += length;
        }
        if w >= h {
            x += thickness;
            w -= thickness;
        } else {
            y += thickness;
            h -= thickness;
        }
        start = end;
    }
    out
}

/// Worst aspect ratio of a row with total area `sum` laid along `side`.
fn worst_ratio(sum: f64, largest: f64, smallest: f64, side: f64) -> f64 {
    let side2 = side * side;
    let sum2 = sum * sum;
    (side2 * largest / sum2).max(sum2 / (side2 * smallest))
}

#[allow(clippy::cast_possible_truncation)]
fn rect_of(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect::from_min_max(
        pos2(x as f32, y as f32),
        pos2((x + w) as f32, (y + h) as f32),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(r: Rect) -> f64 {
        f64::from(r.width()) * f64::from(r.height())
    }

    #[test]
    fn areas_are_proportional_and_inside() {
        let rect = Rect::from_min_max(pos2(10.0, 20.0), pos2(610.0, 420.0));
        let sizes = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0, 0.0];
        let rects = squarify(&sizes, rect);
        let total: f64 = sizes.iter().sum();
        for (r, s) in rects.iter().zip(sizes) {
            if s == 0.0 {
                assert!(!r.is_positive());
                continue;
            }
            let expected = area(rect) * s / total;
            assert!((area(*r) - expected).abs() < expected * 1e-3, "{r:?}");
            assert!(rect.expand(0.01).contains_rect(*r), "{r:?}");
            let ratio = r.width().max(r.height()) / r.width().min(r.height());
            assert!(ratio < 4.0, "{r:?} ratio {ratio}");
        }
        // No overlaps.
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                let overlap = a.intersect(*b);
                assert!(
                    !overlap.is_positive() || area(overlap) < 1e-3,
                    "{a:?} {b:?}"
                );
            }
        }
    }

    #[test]
    fn degenerate_inputs() {
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 50.0));
        assert!(squarify(&[], rect).is_empty());
        assert!(squarify(&[0.0, 0.0], rect).iter().all(|r| !r.is_positive()));
        let one = squarify(&[5.0], rect);
        assert!((area(one[0]) - 5000.0).abs() < 1e-3);
        let empty = Rect::from_min_max(pos2(0.0, 0.0), pos2(0.0, 50.0));
        assert!(squarify(&[1.0], empty).iter().all(|r| !r.is_positive()));
    }
}
