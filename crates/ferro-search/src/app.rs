//! The search window UI (SPEC §4).

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, Event, FontId, Key, Modifiers, Rect, RichText, ScrollArea, Sense,
    TextEdit, vec2,
};
use ferro_core::index::flags;
use ferro_ipc::{Row, SearchRequest, Sort};

use crate::format::{format_size, format_time};
use crate::shell;
use crate::window::WindowControl;
use crate::worker::{Job, Outcome, ServiceState};

/// Delay between the last keystroke and sending the query.
const DEBOUNCE: Duration = Duration::from_millis(30);
/// Rows fetched per request; more are fetched while scrolling.
const PAGE: u32 = 500;
const ROW_HEIGHT: f32 = 24.0;

pub struct App {
    control: Arc<WindowControl>,
    hwnd: Option<isize>,
    jobs: Sender<Job>,
    outcomes: Receiver<Outcome>,

    query: String,
    edited_at: Option<Instant>,
    generation: u64,
    sort: Sort,
    descending: bool,

    rows: Vec<Row>,
    total: u64,
    took_us: u64,
    complete: bool,
    loading_more: bool,
    message: Option<String>,
    service: ServiceState,

    selected: usize,
    scroll_y: f32,
    viewport_height: f32,
    pending_scroll: Option<f32>,
    focus_input: bool,
    was_focused: bool,
    /// Hide the window that eframe shows after the first frame.
    hide_after_first_frame: bool,
}

impl App {
    pub fn new(
        control: Arc<WindowControl>,
        hwnd: Option<isize>,
        jobs: Sender<Job>,
        outcomes: Receiver<Outcome>,
        start_hidden: bool,
    ) -> Self {
        Self {
            control,
            hwnd,
            jobs,
            outcomes,
            query: String::new(),
            edited_at: None,
            generation: 0,
            sort: Sort::Relevance,
            descending: false,
            rows: Vec::new(),
            total: 0,
            took_us: 0,
            complete: true,
            loading_more: false,
            message: None,
            service: ServiceState::Connected,
            selected: 0,
            scroll_y: 0.0,
            viewport_height: 0.0,
            pending_scroll: None,
            focus_input: true,
            was_focused: false,
            hide_after_first_frame: start_hidden,
        }
    }

    fn request(&self, offset: u32) -> SearchRequest {
        SearchRequest {
            query: self.query.trim().to_owned(),
            sort: self.sort,
            descending: self.descending,
            offset,
            limit: PAGE,
        }
    }

    /// Starts a new search; results of older searches are ignored from now on.
    fn submit(&mut self) {
        self.edited_at = None;
        self.generation += 1;
        self.loading_more = false;
        if self.query.trim().is_empty() {
            self.rows.clear();
            self.total = 0;
            self.message = None;
            return;
        }
        let job = Job {
            generation: self.generation,
            request: self.request(0),
            append: false,
        };
        let _ = self.jobs.send(job);
    }

    fn fetch_more(&mut self) {
        if self.loading_more || self.rows.len() as u64 >= self.total {
            return;
        }
        self.loading_more = true;
        let offset = u32::try_from(self.rows.len()).unwrap_or(u32::MAX);
        let job = Job {
            generation: self.generation,
            request: self.request(offset),
            append: true,
        };
        let _ = self.jobs.send(job);
    }

    fn receive(&mut self) {
        while let Ok(outcome) = self.outcomes.try_recv() {
            self.service = outcome.service;
            if outcome.generation != self.generation {
                continue; // a newer search has started
            }
            if outcome.append {
                self.loading_more = false;
            }
            match outcome.result {
                Ok(result) => {
                    if outcome.append {
                        self.rows.extend(result.rows);
                    } else {
                        self.rows = result.rows;
                        self.selected = 0;
                        self.pending_scroll = Some(0.0);
                    }
                    self.total = result.total;
                    self.took_us = result.took_us;
                    self.complete = result.complete;
                    self.message = None;
                }
                Err(message) => self.message = Some(message),
            }
        }
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
        let top = self.selected as f32 * ROW_HEIGHT;
        if top < self.scroll_y {
            self.pending_scroll = Some(top);
        } else if top + ROW_HEIGHT > self.scroll_y + self.viewport_height {
            self.pending_scroll = Some(top + ROW_HEIGHT - self.viewport_height);
        }
    }

    fn page_rows(&self) -> isize {
        ((self.viewport_height / ROW_HEIGHT) as isize - 1).max(1)
    }

    fn act(&mut self, action: Action) {
        let Some(path) = self.selected_row().map(|row| row.path.clone()) else {
            return;
        };
        let result = match action {
            Action::Open => {
                self.control.hide();
                shell::open(&path)
            }
            Action::Reveal => {
                self.control.hide();
                shell::reveal(&path)
            }
            Action::CopyFile => shell::copy_file(self.hwnd, &path)
                .map(|()| self.message = Some(format!("Copied file {path}"))),
        };
        if let Err(err) = result {
            tracing::warn!("{err:#}");
            self.message = Some(format!("{err:#}"));
        }
    }

    /// Keyboard shortcuts, handled before the text field sees the keys.
    fn handle_keys(&mut self, ctx: &egui::Context) {
        let mut actions = Vec::new();
        ctx.input_mut(|input| {
            if input.consume_key(Modifiers::NONE, Key::Escape) {
                actions.push(KeyAction::Hide);
            }
            if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                actions.push(KeyAction::Move(1));
            }
            if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                actions.push(KeyAction::Move(-1));
            }
            if input.consume_key(Modifiers::NONE, Key::PageDown) {
                actions.push(KeyAction::Page(1));
            }
            if input.consume_key(Modifiers::NONE, Key::PageUp) {
                actions.push(KeyAction::Page(-1));
            }
            if input.consume_key(Modifiers::COMMAND, Key::Enter) {
                actions.push(KeyAction::Do(Action::Reveal));
            }
            if input.consume_key(Modifiers::NONE, Key::Enter) {
                actions.push(KeyAction::Do(Action::Open));
            }
            // Ctrl+C arrives as a Copy event; with a selected row it copies the path
            // (Ctrl+Shift+C copies the file itself).
            let shift = input.modifiers.shift;
            let before = input.events.len();
            input.events.retain(|e| !matches!(e, Event::Copy));
            if input.events.len() != before {
                actions.push(if shift {
                    KeyAction::Do(Action::CopyFile)
                } else {
                    KeyAction::CopyPath
                });
            }
        });
        for action in actions {
            match action {
                KeyAction::Hide => self.control.hide(),
                KeyAction::Move(d) => self.move_selection(d),
                KeyAction::Page(d) => self.move_selection(d * self.page_rows()),
                KeyAction::Do(a) => self.act(a),
                KeyAction::CopyPath => {
                    if let Some(row) = self.selected_row() {
                        let path = row.path.clone();
                        ctx.copy_text(path.clone());
                        self.message = Some(format!("Copied {path}"));
                    }
                }
            }
        }
    }

    fn status_line(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let weak = ui.visuals().weak_text_color();
            if let Some(message) = &self.message {
                ui.label(RichText::new(message).color(ui.visuals().warn_fg_color));
            } else if !self.query.trim().is_empty() {
                let mut text = format!(
                    "{} results · {:.1} ms",
                    group_digits(self.total),
                    self.took_us as f64 / 1000.0
                );
                if !self.complete {
                    text.push_str(" · indexing…");
                }
                ui.label(RichText::new(text).color(weak));
            } else {
                ui.label(RichText::new("Enter open · Ctrl+Enter reveal · Ctrl+C copy path · Ctrl+Shift+C copy file · Esc hide").color(weak));
            }
        });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
        let mut clicked = None;
        for (sort, title, column) in [
            (Sort::Name, "Name", Column::Name),
            (Sort::Relevance, "Folder", Column::Path),
            (Sort::Size, "Size", Column::Size),
            (Sort::Modified, "Modified", Column::Modified),
        ] {
            let cell = column.rect(rect);
            let response = ui.interact(cell, ui.id().with(title), Sense::click());
            let arrow = match (
                self.sort == sort && sort != Sort::Relevance,
                self.descending,
            ) {
                (true, false) => " ▲",
                (true, true) => " ▼",
                _ => "",
            };
            let color = if response.hovered() {
                ui.visuals().strong_text_color()
            } else {
                ui.visuals().weak_text_color()
            };
            ui.painter().with_clip_rect(cell).text(
                column.anchor(cell),
                column.align(),
                format!("{title}{arrow}"),
                FontId::proportional(12.0),
                color,
            );
            if response.clicked() {
                clicked = Some(sort);
            }
        }
        if let Some(sort) = clicked {
            if self.sort == sort && sort != Sort::Relevance {
                self.descending = !self.descending;
            } else {
                self.sort = sort;
                self.descending = matches!(sort, Sort::Size | Sort::Modified);
            }
            self.submit();
        }
    }

    fn results(&mut self, ui: &mut egui::Ui) {
        let mut area = ScrollArea::vertical().auto_shrink(false).id_salt("results");
        if let Some(offset) = self.pending_scroll.take() {
            area = area.vertical_scroll_offset(offset);
        }
        let mut open = false;
        let mut clicked = None;
        let mut near_end = false;
        let output = area.show_rows(ui, ROW_HEIGHT, self.rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            near_end = range.end + 50 >= self.rows.len();
            for i in range {
                let row = &self.rows[i];
                let (rect, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
                paint_row(ui, rect, row, i == self.selected, response.hovered());
                if response.clicked() {
                    clicked = Some(i);
                }
                if response.double_clicked() {
                    clicked = Some(i);
                    open = true;
                }
            }
        });
        self.scroll_y = output.state.offset.y;
        self.viewport_height = output.inner_rect.height();
        if let Some(i) = clicked {
            self.selected = i;
        }
        if open {
            self.act(Action::Open);
        }
        if near_end {
            self.fetch_more();
        }
    }

    fn service_missing(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            let (title, detail) = match &self.service {
                ServiceState::NotRunning => (
                    "The ferrofs service is not running.",
                    "Install it from an elevated shell: ferro-service install".to_owned(),
                ),
                ServiceState::Unavailable(reason) => {
                    ("Cannot use the ferrofs service.", reason.clone())
                }
                ServiceState::Connected => return,
            };
            ui.label(RichText::new(title).size(16.0).strong());
            ui.label(RichText::new(detail).color(ui.visuals().weak_text_color()));
            ui.add_space(8.0);
            if ui.button("Retry").clicked() {
                self.service = ServiceState::Connected;
                self.submit();
            }
        });
    }
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Open,
    Reveal,
    CopyFile,
}

enum KeyAction {
    Hide,
    Move(isize),
    Page(isize),
    Do(Action),
    CopyPath,
}

#[derive(Debug, Clone, Copy)]
enum Column {
    Name,
    Path,
    Size,
    Modified,
}

impl Column {
    /// Horizontal share of the row: name 32 %, folder 40 %, size 10 %, modified 18 %.
    fn rect(self, row: Rect) -> Rect {
        let (from, to) = match self {
            Self::Name => (0.0, 0.32),
            Self::Path => (0.32, 0.72),
            Self::Size => (0.72, 0.82),
            Self::Modified => (0.82, 1.0),
        };
        let x = |f: f32| row.left() + row.width() * f;
        Rect::from_x_y_ranges(x(from) + 6.0..=x(to) - 6.0, row.y_range())
    }

    fn align(self) -> Align2 {
        match self {
            Self::Size => Align2::RIGHT_CENTER,
            _ => Align2::LEFT_CENTER,
        }
    }

    fn anchor(self, cell: Rect) -> egui::Pos2 {
        match self {
            Self::Size => cell.right_center(),
            _ => cell.left_center(),
        }
    }
}

fn paint_row(ui: &egui::Ui, rect: Rect, row: &Row, selected: bool, hovered: bool) {
    let visuals = ui.visuals();
    if selected {
        ui.painter()
            .rect_filled(rect, 3.0, visuals.selection.bg_fill);
    } else if hovered {
        ui.painter()
            .rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let strong = if selected {
        visuals.selection.stroke.color
    } else {
        visuals.strong_text_color()
    };
    let weak = if selected {
        visuals.selection.stroke.color
    } else {
        visuals.weak_text_color()
    };

    let (folder, name) = row.path.rsplit_once('\\').unwrap_or(("", &row.path));
    let is_dir = row.flags & flags::DIR != 0;
    let name = if is_dir {
        format!("{name}\\")
    } else {
        name.to_owned()
    };
    let size = if is_dir {
        String::new()
    } else {
        format_size(row.size)
    };
    let cells = [
        (Column::Name, name, strong),
        (Column::Path, folder.to_owned(), weak),
        (Column::Size, size, weak),
        (Column::Modified, format_time(row.mtime), weak),
    ];
    let font = FontId::proportional(13.5);
    for (column, text, color) in cells {
        let cell = column.rect(rect);
        // Folders keep their informative tail ("…\srcpp"), other cells their start.
        let keep_end = matches!(column, Column::Path);
        let text = fit_text(ui, &text, &font, cell.width(), keep_end);
        ui.painter().with_clip_rect(cell).text(
            column.anchor(cell),
            column.align(),
            text,
            font.clone(),
            color,
        );
    }
}

/// Shortens `text` with an ellipsis so it fits `width`, keeping its end or its start.
fn fit_text(ui: &egui::Ui, text: &str, font: &FontId, width: f32, keep_end: bool) -> String {
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
    // Largest number of kept characters that still fits (binary search).
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

fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.hide_after_first_frame) {
            // Runs after eframe's post-first-frame `set_visible(true)`.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.request_repaint();
        }
        if let Some(shown_at) = self.control.take_shown_at() {
            tracing::info!(
                ms = shown_at.elapsed().as_secs_f64() * 1000.0,
                "frame after toggle"
            );
            self.focus_input = true;
        }

        // Hide when another window takes the focus (like a launcher).
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
        if self.was_focused && !focused && self.control.is_visible() {
            self.control.hide();
        }
        self.was_focused = focused;

        self.receive();
        self.handle_keys(ctx);

        let frame = egui::Frame::central_panel(&ctx.style()).inner_margin(12.0);
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let input = TextEdit::singleline(&mut self.query)
                .hint_text("Search files and folders…")
                .font(FontId::proportional(20.0))
                .desired_width(f32::INFINITY)
                .margin(vec2(8.0, 6.0));
            let response = ui.add(input);
            if self.focus_input {
                response.request_focus();
                self.focus_input = false;
            }
            if response.changed() {
                self.edited_at = Some(Instant::now());
            }
            ui.add_space(4.0);
            self.status_line(ui);
            ui.separator();

            if self.service != ServiceState::Connected {
                self.service_missing(ui);
                return;
            }
            self.header(ui);
            self.results(ui);
        });

        if let Some(edited_at) = self.edited_at {
            let waited = edited_at.elapsed();
            if waited >= DEBOUNCE {
                self.submit();
            } else {
                ctx.request_repaint_after(DEBOUNCE - waited);
            }
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        let c: Color32 = visuals.panel_fill;
        egui::Rgba::from(c).to_array()
    }
}

#[cfg(test)]
mod tests {
    use super::group_digits;

    #[test]
    fn digit_grouping() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_846_519), "1,846,519");
    }
}
