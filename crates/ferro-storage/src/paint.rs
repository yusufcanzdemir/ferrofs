//! Drawing helpers shared by the tools.

use eframe::egui::{self, Color32, FontId, Rect};

/// Selection or hover background of a list row.
pub fn row_background(ui: &egui::Ui, rect: Rect, selected: bool, hovered: bool) {
    let visuals = ui.visuals();
    if selected {
        ui.painter()
            .rect_filled(rect, 3.0, visuals.selection.bg_fill);
    } else if hovered {
        ui.painter()
            .rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
    }
}

/// Shortens `text` with an ellipsis so it fits `width`, keeping its end or its start.
pub fn fit_text(ui: &egui::Ui, text: &str, font: &FontId, width: f32, keep_end: bool) -> String {
    let measure = |s: &str| {
        ui.fonts(|fonts| {
            fonts
                .layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE)
                .size()
                .x
        })
    };
    if measure(text) <= width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let candidate = |keep: usize| -> String {
        if keep_end {
            std::iter::once('…')
                .chain(chars[chars.len() - keep..].iter().copied())
                .collect()
        } else {
            chars[..keep]
                .iter()
                .copied()
                .chain(std::iter::once('…'))
                .collect()
        }
    };
    let (mut low, mut high) = (0, chars.len());
    while low < high {
        let mid = (low + high).div_ceil(2);
        if measure(&candidate(mid)) <= width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    candidate(low)
}
