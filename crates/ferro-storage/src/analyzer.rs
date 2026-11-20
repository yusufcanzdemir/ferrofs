//! The analyzer tool (SPEC §5.1): a tree table of the current folder with lazily loaded
//! sub-folders, a squarified treemap, and the largest files and extensions below it.

use std::sync::mpsc::{Receiver, Sender};

use eframe::egui::{
    self, Align2, Color32, FontId, Key, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, pos2,
    vec2,
};
use ferro_core::index::flags;
use ferro_ipc::{ExtensionRow, Request, Response, Row, SizeRest, SizeTreeRequest};

use crate::format;
use crate::paint::{fit_text, row_background as paint_row_background};
use crate::shell;
use crate::tree::{self, Measure, Node};
use crate::treemap::squarify;
use crate::worker::{Job, JobKind, Outcome, ServiceState};

const ROW_HEIGHT: f32 = 22.0;
/// Children per folder when loading a location (two levels, for the treemap).
const NAVIGATE_CHILDREN: u32 = 150;
/// Children loaded when a folder is expanded in the tree.
const EXPAND_CHILDREN: u32 = 1_000;
const TOP_FILES: u32 = 100;
const TOP_EXTENSIONS: u32 = 50;
/// Deepest nesting drawn in the treemap; folders are drawn nested (and their children
/// loaded on demand) as long as their tile has room, up to this depth.
const TREEMAP_DEPTH: usize = 16;
/// Most folders the treemap asks the service for per frame.
const TREEMAP_LOADS_PER_FRAME: usize = 16;
const INDENT: f32 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportTab {
    Largest,
    Extensions,
}

/// Something the user did this frame; applied after drawing, because the views borrow
/// the tree.
#[derive(Debug, Clone)]
enum Action {
    Select(String),
    Navigate(String),
    Toggle(String),
    Reveal(String),
    Open(String),
    CopyPath(String),
    FilterExtension(String),
}

pub struct Analyzer {
    jobs: Sender<Job>,
    outcomes: Receiver<Outcome>,
    service: ServiceState,
    /// Bumped on navigation and filter changes; older responses are ignored.
    generation: u64,
    /// Current folder (empty = all volumes).
    location: String,
    back: Vec<String>,
    path_input: String,
    filter_input: String,
    /// Applied filter query.
    filter: String,
    measure: Measure,
    root: Option<Node>,
    largest: Vec<Row>,
    extensions: Vec<ExtensionRow>,
    took_us: u64,
    complete: bool,
    loading: bool,
    error: Option<String>,
    /// Transient feedback such as "Copied".
    message: Option<String>,
    selected: Option<String>,
    tab: ReportTab,
    /// Treemap item under the pointer when its context menu was opened.
    context_target: Option<Target>,
    actions: Vec<Action>,
    /// Side panels have been sized to the real window (the first frames can run before
    /// the window has its final size).
    panels_sized: bool,
}

/// A treemap item the pointer is over.
#[derive(Debug, Clone)]
struct Target {
    path: String,
    is_dir: bool,
}

impl Analyzer {
    pub fn new(jobs: Sender<Job>, outcomes: Receiver<Outcome>, start: String) -> Self {
        let mut app = Self {
            jobs,
            outcomes,
            service: ServiceState::Connected,
            generation: 0,
            location: String::new(),
            back: Vec::new(),
            path_input: String::new(),
            filter_input: String::new(),
            filter: String::new(),
            measure: Measure::Size,
            root: None,
            largest: Vec::new(),
            extensions: Vec::new(),
            took_us: 0,
            complete: true,
            loading: false,
            error: None,
            message: None,
            selected: None,
            tab: ReportTab::Largest,
            context_target: None,
            actions: Vec::new(),
            panels_sized: false,
        };
        app.navigate(start, false);
        app
    }

    fn send(&mut self, kind: JobKind, request: SizeTreeRequest) {
        let job = Job {
            generation: self.generation,
            kind,
            request: Request::SizeTree(request),
        };
        if self.jobs.send(job).is_err() {
            self.error = Some("the background worker stopped".into());
        }
    }

    /// Loads `path` (empty = all volumes) as the current location.
    fn navigate(&mut self, path: String, remember: bool) {
        if remember && path != self.location {
            self.back.push(std::mem::take(&mut self.location));
        }
        self.location.clone_from(&path);
        self.path_input.clone_from(&path);
        self.generation += 1;
        self.loading = true;
        self.error = None;
        let request = SizeTreeRequest {
            root: path,
            depth: 2,
            max_children: NAVIGATE_CHILDREN,
            filter: self.filter.clone(),
            top_files: TOP_FILES,
            top_extensions: TOP_EXTENSIONS,
        };
        self.send(JobKind::Navigate, request);
    }

    fn refresh(&mut self) {
        self.navigate(self.location.clone(), false);
    }

    fn set_filter(&mut self, filter: &str) {
        self.filter_input = filter.to_owned();
        self.filter = filter.trim().to_owned();
        self.refresh();
    }

    /// Expands or collapses the folder at `path`, loading its children on first expand.
    fn toggle(&mut self, path: &str) {
        let Some(node) = self.root.as_mut().and_then(|root| root.find_mut(path)) else {
            return;
        };
        node.expanded = !node.expanded;
        if node.expanded {
            self.load_children(path);
        }
    }

    /// Asks the service for the children of the folder at `path` unless they are loaded or
    /// already requested.
    fn load_children(&mut self, path: &str) {
        let Some(node) = self.root.as_mut().and_then(|root| root.find_mut(path)) else {
            return;
        };
        if node.children.is_some() || node.loading {
            return;
        }
        node.loading = true;
        let request = SizeTreeRequest {
            root: path.to_owned(),
            depth: 1,
            max_children: EXPAND_CHILDREN,
            filter: self.filter.clone(),
            top_files: 0,
            top_extensions: 0,
        };
        self.send(JobKind::Expand(path.to_owned()), request);
    }

    fn receive(&mut self) {
        while let Ok(outcome) = self.outcomes.try_recv() {
            if outcome.generation != self.generation {
                continue;
            }
            self.service = outcome.service;
            let result = outcome.result.and_then(|response| match response {
                Response::SizeTree(tree) => Ok(tree),
                other => Err(format!("unexpected response {other:?}")),
            });
            match (outcome.kind, result) {
                (JobKind::Navigate, Ok(tree)) => {
                    self.loading = false;
                    self.error = None;
                    let mut root = Node::from_ipc(tree.root, tree.path.clone());
                    root.expanded = true;
                    root.sort(self.measure);
                    self.root = Some(root);
                    self.location.clone_from(&tree.path);
                    self.path_input = tree.path;
                    self.largest = tree.largest;
                    self.extensions = tree.extensions;
                    self.took_us = tree.took_us;
                    self.complete = tree.complete;
                }
                (JobKind::Navigate, Err(message)) => {
                    self.loading = false;
                    self.error = Some(message);
                }
                (JobKind::Expand(path), result) => {
                    let measure = self.measure;
                    let Some(node) = self.root.as_mut().and_then(|root| root.find_mut(&path))
                    else {
                        continue;
                    };
                    match result {
                        Ok(tree) => {
                            let mut loaded = Node::from_ipc(tree.root, path);
                            loaded.sort(measure);
                            node.set_children(loaded);
                        }
                        Err(message) => {
                            // Shown as empty until the next refresh, so the treemap does
                            // not ask again on every frame.
                            node.loading = false;
                            node.expanded = false;
                            node.children = Some(Vec::new());
                            self.error = Some(message);
                        }
                    }
                }
                (
                    JobKind::Duplicates
                    | JobKind::Artifacts
                    | JobKind::Location(_)
                    | JobKind::VirtualDisks,
                    _,
                ) => {}
            }
        }
    }

    fn apply_actions(&mut self, ctx: &egui::Context) {
        for action in std::mem::take(&mut self.actions) {
            self.message = None;
            match action {
                Action::Select(path) => self.selected = Some(path),
                Action::Navigate(path) => self.navigate(path, true),
                Action::Toggle(path) => self.toggle(&path),
                Action::Reveal(path) => {
                    if let Err(err) = shell::reveal(&path) {
                        self.error = Some(format!("{err:#}"));
                    }
                }
                Action::Open(path) => {
                    if let Err(err) = shell::open(&path) {
                        self.error = Some(format!("{err:#}"));
                    }
                }
                Action::CopyPath(path) => {
                    ctx.copy_text(path.clone());
                    self.message = Some(format!("Copied {path}"));
                }
                Action::FilterExtension(ext) => {
                    let filter = format!("ext:{ext}");
                    if self.filter.eq_ignore_ascii_case(&filter) {
                        self.set_filter("");
                    } else {
                        self.set_filter(&filter);
                    }
                }
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let typing = ctx.memory(|m| m.focused().is_some());
        let (back, up, refresh) = ctx.input(|i| {
            (
                i.modifiers.alt && i.key_pressed(Key::ArrowLeft),
                !typing && i.key_pressed(Key::Backspace),
                i.key_pressed(Key::F5),
            )
        });
        if back {
            self.go_back();
        } else if up {
            self.go_up();
        } else if refresh {
            self.refresh();
        }
    }

    fn go_back(&mut self) {
        if let Some(previous) = self.back.pop() {
            self.navigate(previous, false);
        }
    }

    fn go_up(&mut self) {
        if !self.location.is_empty() {
            let parent = tree::parent(&self.location);
            self.navigate(parent, true);
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.back.is_empty(), egui::Button::new("Back"))
                .on_hover_text("Alt+Left")
                .clicked()
            {
                self.go_back();
            }
            if ui
                .add_enabled(!self.location.is_empty(), egui::Button::new("Up"))
                .on_hover_text("Backspace")
                .clicked()
            {
                self.go_up();
            }
            if ui.button("Refresh").on_hover_text("F5").clicked() {
                self.refresh();
            }
            ui.separator();
            if ui
                .selectable_label(self.location.is_empty(), "This PC")
                .clicked()
            {
                self.actions.push(Action::Navigate(String::new()));
            }
            for (name, path) in tree::breadcrumbs(&self.location) {
                ui.label(RichText::new("›").weak());
                let current = path.eq_ignore_ascii_case(&self.location);
                if ui.selectable_label(current, name).clicked() && !current {
                    self.actions.push(Action::Navigate(path));
                }
            }
        });
        ui.horizontal(|ui| {
            let path = ui.add(
                TextEdit::singleline(&mut self.path_input)
                    .hint_text(r"Folder, e.g. C:\Users")
                    .desired_width(320.0),
            );
            if path.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                let target = self.path_input.trim().to_owned();
                self.actions.push(Action::Navigate(target));
            }
            let filter = ui.add(
                TextEdit::singleline(&mut self.filter_input)
                    .hint_text("Filter files: ext:mp4;mkv  size:>100mb  dm:year")
                    .desired_width(320.0),
            );
            if filter.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                let text = self.filter_input.clone();
                self.set_filter(&text);
            }
            if !self.filter.is_empty() && ui.button("Clear filter").clicked() {
                self.set_filter("");
            }
            ui.separator();
            let before = self.measure;
            ui.selectable_value(&mut self.measure, Measure::Size, "Size");
            ui.selectable_value(&mut self.measure, Measure::OnDisk, "On disk")
                .on_hover_text("Allocated bytes: smaller for compressed and sparse files");
            if self.measure != before
                && let Some(root) = &mut self.root
            {
                root.sort(self.measure);
            }
        });
        ui.add_space(2.0);
    }

    fn status(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.loading {
                ui.add(egui::Spinner::new().size(12.0));
            }
            if let Some(root) = &self.root {
                let place = if root.path.is_empty() {
                    "All volumes"
                } else {
                    root.path.as_str()
                };
                ui.label(format!(
                    "{place}: {} ({} on disk), {} files, {} folders, {:.0} ms",
                    format::size(root.size),
                    format::size(root.alloc_size),
                    format::count(root.files),
                    format::count(root.folders),
                    self.took_us as f64 / 1000.0
                ));
            }
            if !self.filter.is_empty() {
                ui.label(RichText::new(format!("filter: {}", self.filter)).strong());
            }
            if !self.complete {
                ui.label(RichText::new("indexing not finished").color(ui.visuals().warn_fg_color));
            }
            if let Some(error) = &self.error {
                ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            } else if let Some(message) = &self.message {
                ui.label(RichText::new(message).weak());
            }
        });
    }

    fn service_missing(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
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
                self.refresh();
            }
        });
    }

    fn tree_view(&mut self, ui: &mut egui::Ui) {
        let Self {
            root,
            actions,
            selected,
            measure,
            ..
        } = self;
        let Some(root) = root.as_ref() else {
            return;
        };
        let measure = *measure;
        tree_header(ui, measure);
        let mut rows = Vec::new();
        flatten(root, 0, measure, &mut rows);
        ScrollArea::vertical().auto_shrink([false; 2]).show_rows(
            ui,
            ROW_HEIGHT,
            rows.len(),
            |ui, range| {
                for row in &rows[range] {
                    let (rect, response) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ROW_HEIGHT),
                        Sense::click(),
                    );
                    match *row {
                        Flat::Node {
                            depth,
                            node,
                            parent_total,
                        } => {
                            let is_selected = selected.as_deref() == Some(node.path.as_str());
                            paint_tree_row(
                                ui,
                                rect,
                                depth,
                                node,
                                parent_total,
                                measure,
                                is_selected,
                                response.hovered(),
                            );
                            let expander = expander_rect(rect, depth);
                            let on_expander = response
                                .interact_pointer_pos()
                                .is_some_and(|p| expander.expand(3.0).contains(p));
                            if response.clicked() {
                                if on_expander && node.has_children() {
                                    actions.push(Action::Toggle(node.path.clone()));
                                } else {
                                    actions.push(Action::Select(node.path.clone()));
                                }
                            }
                            if response.double_clicked() && !on_expander {
                                actions.push(if node.is_dir() {
                                    Action::Navigate(node.path.clone())
                                } else {
                                    Action::Reveal(node.path.clone())
                                });
                            }
                            response.context_menu(|ui| {
                                item_menu(ui, &node.path, node.is_dir(), actions);
                            });
                        }
                        Flat::Rest {
                            depth,
                            rest,
                            parent_total,
                        } => {
                            paint_rest_row(ui, rect, depth, rest, parent_total, measure);
                        }
                        Flat::Loading { depth } => {
                            let x = rect.left() + 6.0 + depth as f32 * INDENT + INDENT;
                            ui.painter().text(
                                pos2(x, rect.center().y),
                                Align2::LEFT_CENTER,
                                "Loading…",
                                FontId::proportional(13.0),
                                ui.visuals().weak_text_color(),
                            );
                        }
                    }
                }
            },
        );
    }

    fn treemap_view(&mut self, ui: &mut egui::Ui) {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click());
        let Some(root) = &self.root else {
            return;
        };
        let painter = ui.painter_at(rect);
        let style = TreemapStyle::new(ui.visuals().dark_mode, ui.visuals());
        let mut hovered = None;
        let mut wanted = Vec::new();
        paint_treemap(
            &painter,
            &style,
            rect,
            root,
            0,
            self.measure,
            response.hover_pos(),
            self.selected.as_deref(),
            &mut hovered,
            &mut wanted,
        );
        if root.children.as_ref().is_some_and(Vec::is_empty) && root.rest.is_none() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Nothing here",
                FontId::proportional(15.0),
                ui.visuals().weak_text_color(),
            );
        }
        if let Some(item) = &hovered {
            let measure = self.measure;
            response.clone().on_hover_ui_at_pointer(|ui| {
                ui.label(RichText::new(&item.path).strong());
                ui.label(format!(
                    "{} ({} on disk)",
                    format::size(item.size),
                    format::size(item.alloc_size)
                ));
                if item.is_dir {
                    ui.label(format!(
                        "{} files, {} folders",
                        format::count(item.files),
                        format::count(item.folders)
                    ));
                }
                let _ = measure;
            });
        }
        if response.clicked()
            && let Some(item) = &hovered
        {
            self.actions.push(Action::Select(item.path.clone()));
        }
        if response.double_clicked()
            && let Some(item) = &hovered
        {
            self.actions.push(if item.is_dir {
                Action::Navigate(item.path.clone())
            } else {
                Action::Reveal(item.path.clone())
            });
        }
        if response.secondary_clicked() {
            self.context_target = hovered.as_ref().map(|item| Target {
                path: item.path.clone(),
                is_dir: item.is_dir,
            });
        }
        for path in wanted.into_iter().take(TREEMAP_LOADS_PER_FRAME) {
            self.load_children(&path);
        }
        let target = self.context_target.clone();
        let actions = &mut self.actions;
        response.context_menu(|ui| match &target {
            Some(target) => item_menu(ui, &target.path, target.is_dir, actions),
            None => {
                ui.label("Nothing selected");
            }
        });
    }

    fn reports(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, ReportTab::Largest, "Largest files");
            ui.selectable_value(&mut self.tab, ReportTab::Extensions, "Extensions");
        });
        ui.separator();
        match self.tab {
            ReportTab::Largest => self.largest_view(ui),
            ReportTab::Extensions => self.extensions_view(ui),
        }
    }

    fn largest_view(&mut self, ui: &mut egui::Ui) {
        let Self {
            largest,
            actions,
            selected,
            ..
        } = self;
        if largest.is_empty() {
            ui.label(RichText::new("No files").weak());
            return;
        }
        let font = FontId::proportional(13.0);
        ScrollArea::vertical().auto_shrink([false; 2]).show_rows(
            ui,
            ROW_HEIGHT * 1.6,
            largest.len(),
            |ui, range| {
                for row in &largest[range] {
                    let (rect, response) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ROW_HEIGHT * 1.6),
                        Sense::click(),
                    );
                    let is_selected = selected.as_deref() == Some(row.path.as_str());
                    paint_row_background(ui, rect, is_selected, response.hovered());
                    let visuals = ui.visuals();
                    let (name, folder) = match row.path.rsplit_once('\\') {
                        Some((folder, name)) => (name, folder),
                        None => (row.path.as_str(), ""),
                    };
                    let size_text = format::size(row.size);
                    let top = rect.top() + ROW_HEIGHT * 0.45;
                    let bottom = rect.bottom() - ROW_HEIGHT * 0.45;
                    let size_width = 72.0;
                    let text_rect = Rect::from_x_y_ranges(
                        rect.left() + 6.0..=rect.right() - size_width - 6.0,
                        rect.y_range(),
                    );
                    let painter = ui.painter().with_clip_rect(text_rect);
                    painter.text(
                        pos2(text_rect.left(), top),
                        Align2::LEFT_CENTER,
                        fit_text(ui, name, &font, text_rect.width(), false),
                        font.clone(),
                        visuals.strong_text_color(),
                    );
                    painter.text(
                        pos2(text_rect.left(), bottom),
                        Align2::LEFT_CENTER,
                        fit_text(ui, folder, &font, text_rect.width(), true),
                        FontId::proportional(12.0),
                        visuals.weak_text_color(),
                    );
                    ui.painter().text(
                        pos2(rect.right() - 6.0, top),
                        Align2::RIGHT_CENTER,
                        size_text,
                        font.clone(),
                        visuals.text_color(),
                    );
                    let response = response.on_hover_text(&row.path);
                    if response.clicked() {
                        actions.push(Action::Select(row.path.clone()));
                    }
                    if response.double_clicked() {
                        actions.push(Action::Reveal(row.path.clone()));
                    }
                    response.context_menu(|ui| {
                        item_menu(ui, &row.path, row.flags & flags::DIR != 0, actions);
                    });
                }
            },
        );
    }

    fn extensions_view(&mut self, ui: &mut egui::Ui) {
        if self.extensions.is_empty() {
            ui.label(RichText::new("No files").weak());
            return;
        }
        ui.label(RichText::new("Click an extension to show only those files.").weak());
        let total = self
            .root
            .as_ref()
            .map_or(0, |root| root.measure(self.measure))
            .max(1);
        let dark = ui.visuals().dark_mode;
        let font = FontId::proportional(13.0);
        let measure = self.measure;
        let current = self.filter.to_lowercase();
        let Self {
            extensions,
            actions,
            ..
        } = self;
        ScrollArea::vertical().auto_shrink([false; 2]).show_rows(
            ui,
            ROW_HEIGHT,
            extensions.len(),
            |ui, range| {
                for row in &extensions[range] {
                    let (rect, response) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ROW_HEIGHT),
                        Sense::click(),
                    );
                    let active = current == format!("ext:{}", row.ext);
                    paint_row_background(ui, rect, active, response.hovered());
                    let visuals = ui.visuals();
                    let bytes = match measure {
                        Measure::Size => row.size,
                        Measure::OnDisk => row.alloc_size,
                    };
                    // Share bar behind the row.
                    let bar = Rect::from_min_size(
                        pos2(rect.left(), rect.bottom() - 3.0),
                        vec2(rect.width() * format::share(bytes, total), 2.0),
                    );
                    ui.painter()
                        .rect_filled(bar, 0.0, ext_color(&row.ext, dark));
                    let chip = Rect::from_center_size(
                        pos2(rect.left() + 12.0, rect.center().y),
                        vec2(10.0, 10.0),
                    );
                    ui.painter()
                        .rect_filled(chip, 2.0, ext_color(&row.ext, dark));
                    let label = if row.ext.is_empty() {
                        "(no extension)".to_owned()
                    } else {
                        format!(".{}", row.ext)
                    };
                    ui.painter().text(
                        pos2(rect.left() + 24.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        label,
                        font.clone(),
                        visuals.strong_text_color(),
                    );
                    ui.painter().text(
                        pos2(rect.right() - 90.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        format::size(bytes),
                        font.clone(),
                        visuals.text_color(),
                    );
                    ui.painter().text(
                        pos2(rect.right() - 6.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        format!("{} files", format::count(row.files)),
                        FontId::proportional(12.0),
                        visuals.weak_text_color(),
                    );
                    if response.clicked() && !row.ext.is_empty() {
                        actions.push(Action::FilterExtension(row.ext.clone()));
                    }
                }
            },
        );
    }
}

impl Analyzer {
    pub fn update(&mut self, ctx: &egui::Context) {
        self.receive();
        self.handle_keys(ctx);
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| self.toolbar(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status(ui));
        if self.service != ServiceState::Connected {
            egui::CentralPanel::default().show(ctx, |ui| self.service_missing(ui));
            self.apply_actions(ctx);
            return;
        }
        let width = ctx.screen_rect().width();
        let mut reports = egui::SidePanel::right("reports")
            .resizable(true)
            .min_width(240.0);
        let mut tree = egui::SidePanel::left("tree")
            .resizable(true)
            .min_width(360.0);
        if !self.panels_sized && width >= 1000.0 {
            reports = reports.exact_width((width * 0.24).clamp(240.0, 420.0));
            tree = tree.exact_width((width * 0.40).clamp(360.0, 720.0));
            self.panels_sized = true;
        }
        reports.show(ctx, |ui| self.reports(ui));
        tree.show(ctx, |ui| self.tree_view(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.treemap_view(ui));
        self.apply_actions(ctx);
    }
}

/// One visible line of the tree table.
enum Flat<'a> {
    Node {
        depth: usize,
        node: &'a Node,
        parent_total: u64,
    },
    Rest {
        depth: usize,
        rest: &'a SizeRest,
        parent_total: u64,
    },
    Loading {
        depth: usize,
    },
}

/// Visible lines below `node`: its children, the expanded ones recursively.
fn flatten<'a>(node: &'a Node, depth: usize, measure: Measure, out: &mut Vec<Flat<'a>>) {
    let total = node.measure(measure);
    match &node.children {
        Some(children) => {
            for child in children {
                out.push(Flat::Node {
                    depth,
                    node: child,
                    parent_total: total,
                });
                if child.expanded {
                    flatten(child, depth + 1, measure, out);
                }
            }
            if let Some(rest) = &node.rest {
                out.push(Flat::Rest {
                    depth,
                    rest,
                    parent_total: total,
                });
            }
        }
        None if node.loading => out.push(Flat::Loading { depth }),
        None => {}
    }
}

#[derive(Debug, Clone, Copy)]
enum Column {
    Name,
    Size,
    Share,
    Files,
    Modified,
}

impl Column {
    const ALL: [Self; 5] = [
        Self::Name,
        Self::Size,
        Self::Share,
        Self::Files,
        Self::Modified,
    ];

    fn rect(self, row: Rect) -> Rect {
        let (from, to) = match self {
            Self::Name => (0.0, 0.38),
            Self::Size => (0.38, 0.52),
            Self::Share => (0.52, 0.68),
            Self::Files => (0.68, 0.80),
            Self::Modified => (0.80, 1.0),
        };
        let x = |f: f32| row.left() + row.width() * f;
        Rect::from_x_y_ranges(x(from) + 6.0..=x(to) - 6.0, row.y_range())
    }

    fn title(self, measure: Measure) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => match measure {
                Measure::Size => "Size",
                Measure::OnDisk => "On disk",
            },
            Self::Share => "% of parent",
            Self::Files => "Files",
            Self::Modified => "Modified",
        }
    }

    fn right_aligned(self) -> bool {
        matches!(self, Self::Size | Self::Files)
    }
}

fn tree_header(ui: &mut egui::Ui, measure: Measure) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
    let visuals = ui.visuals();
    for column in Column::ALL {
        let cell = column.rect(rect);
        let (anchor, align) = if column.right_aligned() {
            (cell.right_center(), Align2::RIGHT_CENTER)
        } else {
            (cell.left_center(), Align2::LEFT_CENTER)
        };
        ui.painter().with_clip_rect(cell).text(
            anchor,
            align,
            column.title(measure),
            FontId::proportional(12.5),
            visuals.weak_text_color(),
        );
    }
    ui.painter().hline(
        rect.x_range(),
        rect.bottom(),
        Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color),
    );
}

fn expander_rect(row: Rect, depth: usize) -> Rect {
    let x = row.left() + 6.0 + depth as f32 * INDENT;
    Rect::from_min_size(pos2(x, row.center().y - 6.0), vec2(12.0, 12.0))
}

#[allow(clippy::too_many_arguments)]
fn paint_tree_row(
    ui: &egui::Ui,
    rect: Rect,
    depth: usize,
    node: &Node,
    parent_total: u64,
    measure: Measure,
    selected: bool,
    hovered: bool,
) {
    paint_row_background(ui, rect, selected, hovered);
    let visuals = ui.visuals();
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
    let font = FontId::proportional(13.5);
    let painter = ui.painter();

    // Name: expander triangle, colored chip, name.
    let name_cell = Column::Name.rect(rect);
    let expander = expander_rect(rect, depth);
    if node.has_children() {
        let c = expander.center();
        let points = if node.expanded {
            vec![
                pos2(c.x - 4.0, c.y - 2.0),
                pos2(c.x + 4.0, c.y - 2.0),
                pos2(c.x, c.y + 3.0),
            ]
        } else {
            vec![
                pos2(c.x - 2.0, c.y - 4.0),
                pos2(c.x - 2.0, c.y + 4.0),
                pos2(c.x + 3.0, c.y),
            ]
        };
        painter.add(egui::Shape::convex_polygon(points, weak, Stroke::NONE));
    }
    let chip = Rect::from_center_size(
        pos2(expander.right() + 9.0, rect.center().y),
        vec2(10.0, 10.0),
    );
    let dark = visuals.dark_mode;
    let chip_color = if node.is_dir() {
        folder_chip_color(dark)
    } else {
        ext_color(&tree::extension(&node.name), dark)
    };
    painter.rect_filled(chip, 2.0, chip_color);
    let text_left = chip.right() + 6.0;
    let name_rect = Rect::from_x_y_ranges(text_left..=name_cell.right(), rect.y_range());
    if name_rect.width() > 4.0 {
        painter.with_clip_rect(name_rect).text(
            name_rect.left_center(),
            Align2::LEFT_CENTER,
            fit_text(ui, &node.name, &font, name_rect.width(), false),
            font.clone(),
            strong,
        );
    }

    let bytes = node.measure(measure);
    let size_cell = Column::Size.rect(rect);
    painter.with_clip_rect(size_cell).text(
        size_cell.right_center(),
        Align2::RIGHT_CENTER,
        format::size(bytes),
        font.clone(),
        strong,
    );
    paint_share(ui, Column::Share.rect(rect), bytes, parent_total, weak);
    if node.is_dir() {
        let files = Column::Files.rect(rect);
        painter.with_clip_rect(files).text(
            files.right_center(),
            Align2::RIGHT_CENTER,
            format::count(node.files),
            font.clone(),
            weak,
        );
    }
    let modified = Column::Modified.rect(rect);
    painter.with_clip_rect(modified).text(
        modified.left_center(),
        Align2::LEFT_CENTER,
        format::date(node.mtime),
        font,
        weak,
    );
}

fn paint_rest_row(
    ui: &egui::Ui,
    rect: Rect,
    depth: usize,
    rest: &SizeRest,
    parent_total: u64,
    measure: Measure,
) {
    let visuals = ui.visuals();
    let weak = visuals.weak_text_color();
    let font = FontId::proportional(13.0);
    let name_cell = Column::Name.rect(rect);
    let left = expander_rect(rect, depth).right() + 20.0;
    let label = format!("{} smaller items", format::count(u64::from(rest.count)));
    let name_rect = Rect::from_x_y_ranges(left..=name_cell.right(), rect.y_range());
    if name_rect.width() > 4.0 {
        ui.painter().with_clip_rect(name_rect).text(
            name_rect.left_center(),
            Align2::LEFT_CENTER,
            label,
            font.clone(),
            weak,
        );
    }
    let bytes = match measure {
        Measure::Size => rest.size,
        Measure::OnDisk => rest.alloc_size,
    };
    let size_cell = Column::Size.rect(rect);
    ui.painter().with_clip_rect(size_cell).text(
        size_cell.right_center(),
        Align2::RIGHT_CENTER,
        format::size(bytes),
        font,
        weak,
    );
    paint_share(ui, Column::Share.rect(rect), bytes, parent_total, weak);
}

/// Bar plus percentage of the parent's size.
fn paint_share(ui: &egui::Ui, cell: Rect, bytes: u64, total: u64, text: Color32) {
    if cell.width() < 8.0 {
        return;
    }
    let share = format::share(bytes, total);
    let visuals = ui.visuals();
    let track = Rect::from_x_y_ranges(
        cell.left()..=cell.left() + (cell.width() * 0.45).min(80.0),
        cell.center().y - 4.0..=cell.center().y + 4.0,
    );
    ui.painter()
        .rect_filled(track, 2.0, visuals.widgets.inactive.bg_fill);
    let mut fill = track;
    fill.set_width(track.width() * share);
    ui.painter()
        .rect_filled(fill, 2.0, visuals.selection.bg_fill);
    ui.painter().with_clip_rect(cell).text(
        pos2(track.right() + 6.0, cell.center().y),
        Align2::LEFT_CENTER,
        format!("{:.1}%", share * 100.0),
        FontId::proportional(12.0),
        text,
    );
}

fn item_menu(ui: &mut egui::Ui, path: &str, is_dir: bool, actions: &mut Vec<Action>) {
    if is_dir && ui.button("Analyze this folder").clicked() {
        actions.push(Action::Navigate(path.to_owned()));
    }
    if ui.button("Show in Explorer").clicked() {
        actions.push(Action::Reveal(path.to_owned()));
    }
    if ui.button("Open").clicked() {
        actions.push(Action::Open(path.to_owned()));
    }
    if ui.button("Copy path").clicked() {
        actions.push(Action::CopyPath(path.to_owned()));
    }
}

/// Colors that depend on the theme.
struct TreemapStyle {
    dark: bool,
    background: Color32,
    border: Color32,
    folder_text: Color32,
    file_text: Color32,
    rest: Color32,
    highlight: Color32,
}

impl TreemapStyle {
    fn new(dark: bool, visuals: &egui::Visuals) -> Self {
        Self {
            dark,
            background: visuals.extreme_bg_color,
            border: visuals.panel_fill,
            folder_text: visuals.strong_text_color(),
            file_text: if dark {
                Color32::from_white_alpha(230)
            } else {
                Color32::from_black_alpha(210)
            },
            rest: if dark {
                Color32::from_gray(70)
            } else {
                Color32::from_gray(190)
            },
            highlight: visuals.selection.stroke.color,
        }
    }

    fn folder(&self, depth: usize) -> Color32 {
        let step = u8::try_from(depth.min(4) * 10).unwrap_or(40);
        if self.dark {
            Color32::from_gray(38 + step)
        } else {
            Color32::from_gray(232 - step)
        }
    }
}

/// What the pointer is over in the treemap.
struct Hovered {
    path: String,
    is_dir: bool,
    size: u64,
    alloc_size: u64,
    files: u64,
    folders: u64,
}

enum Item<'a> {
    Node(&'a Node),
    Rest(&'a SizeRest),
}

/// Lays out the children of `node` inside `rect`, recursing into folders that have room.
#[allow(clippy::too_many_arguments)]
fn paint_treemap(
    painter: &egui::Painter,
    style: &TreemapStyle,
    rect: Rect,
    node: &Node,
    depth: usize,
    measure: Measure,
    pointer: Option<egui::Pos2>,
    selected: Option<&str>,
    hovered: &mut Option<Hovered>,
    wanted: &mut Vec<String>,
) {
    if depth == 0 {
        painter.rect_filled(rect, 0.0, style.background);
    }
    let Some(children) = &node.children else {
        return;
    };
    let mut items: Vec<Item<'_>> = children.iter().map(Item::Node).collect();
    if let Some(rest) = &node.rest {
        items.push(Item::Rest(rest));
    }
    let sizes: Vec<f64> = items
        .iter()
        .map(|item| {
            let bytes = match item {
                Item::Node(n) => n.measure(measure),
                Item::Rest(r) => match measure {
                    Measure::Size => r.size,
                    Measure::OnDisk => r.alloc_size,
                },
            };
            // Treemap areas only need relative sizes.
            #[allow(clippy::cast_precision_loss)]
            let bytes = bytes as f64;
            bytes
        })
        .collect();
    let rects = squarify(&sizes, rect);
    let label_font = FontId::proportional(12.0);
    for (item, &r) in items.iter().zip(&rects) {
        if r.width() < 1.0 || r.height() < 1.0 {
            continue;
        }
        let under_pointer = pointer.is_some_and(|p| r.contains(p));
        match item {
            Item::Node(child) => {
                if under_pointer {
                    *hovered = Some(Hovered {
                        path: child.path.clone(),
                        is_dir: child.is_dir(),
                        size: child.size,
                        alloc_size: child.alloc_size,
                        files: child.files,
                        folders: child.folders,
                    });
                }
                let fill = if child.is_dir() {
                    style.folder(depth)
                } else {
                    ext_color(&tree::extension(&child.name), style.dark)
                };
                painter.rect_filled(r, 0.0, fill);
                painter.rect_stroke(
                    r,
                    0.0,
                    Stroke::new(1.0_f32, style.border),
                    egui::StrokeKind::Inside,
                );
                let room = r.width() > 48.0 && r.height() > 36.0;
                if child.is_dir() && room && depth + 1 < TREEMAP_DEPTH {
                    let header = Rect::from_min_max(r.min, pos2(r.max.x, r.min.y + 16.0));
                    painter.with_clip_rect(header.shrink(2.0)).text(
                        pos2(header.left() + 4.0, header.center().y),
                        Align2::LEFT_CENTER,
                        &child.name,
                        label_font.clone(),
                        style.folder_text,
                    );
                    let inner = Rect::from_min_max(
                        pos2(r.min.x + 2.0, r.min.y + 16.0),
                        pos2(r.max.x - 2.0, r.max.y - 2.0),
                    );
                    if child.children.is_none() && !child.loading && child.has_children() {
                        wanted.push(child.path.clone());
                    }
                    paint_treemap(
                        painter,
                        style,
                        inner,
                        child,
                        depth + 1,
                        measure,
                        pointer,
                        selected,
                        hovered,
                        wanted,
                    );
                } else if r.width() > 40.0 && r.height() > 16.0 {
                    let color = if child.is_dir() {
                        style.folder_text
                    } else {
                        style.file_text
                    };
                    painter.with_clip_rect(r.shrink(2.0)).text(
                        pos2(r.left() + 4.0, r.top() + 9.0),
                        Align2::LEFT_CENTER,
                        &child.name,
                        label_font.clone(),
                        color,
                    );
                }
                if selected.is_some_and(|s| s.eq_ignore_ascii_case(&child.path)) {
                    painter.rect_stroke(
                        r,
                        0.0,
                        Stroke::new(2.5_f32, style.highlight),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            Item::Rest(rest) => {
                painter.rect_filled(r, 0.0, style.rest);
                painter.rect_stroke(
                    r,
                    0.0,
                    Stroke::new(1.0_f32, style.border),
                    egui::StrokeKind::Inside,
                );
                if r.width() > 60.0 && r.height() > 16.0 {
                    painter.with_clip_rect(r.shrink(2.0)).text(
                        pos2(r.left() + 4.0, r.top() + 9.0),
                        Align2::LEFT_CENTER,
                        format!("{} smaller items", format::count(u64::from(rest.count))),
                        label_font.clone(),
                        style.folder_text,
                    );
                }
            }
        }
    }
}

/// Stable color per extension (FNV-1a hash -> hue).
fn ext_color(ext: &str, dark: bool) -> Color32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in ext.bytes() {
        hash = (hash ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    if ext.is_empty() {
        return if dark {
            Color32::from_gray(110)
        } else {
            Color32::from_gray(160)
        };
    }
    // Hue in 0..1 from 360 buckets.
    #[allow(clippy::cast_precision_loss)]
    let hue = (hash % 360) as f32 / 360.0;
    let (saturation, value) = if dark { (0.45, 0.62) } else { (0.40, 0.88) };
    egui::ecolor::Hsva::new(hue, saturation, value, 1.0).into()
}

fn folder_chip_color(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(214, 170, 70)
    } else {
        Color32::from_rgb(230, 180, 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_colors_are_stable_and_distinct() {
        assert_eq!(ext_color("mp4", true), ext_color("mp4", true));
        assert_ne!(ext_color("mp4", false), ext_color("pdf", false));
        assert_ne!(ext_color("", true), ext_color("mp4", true));
    }

    #[test]
    fn flatten_follows_expansion() {
        let mut root = Node::from_ipc(
            ferro_ipc::SizeNode {
                name: r"C:\".into(),
                size: 30,
                flags: flags::DIR,
                child_count: 2,
                children: vec![
                    ferro_ipc::SizeNode {
                        name: "a".into(),
                        size: 20,
                        flags: flags::DIR,
                        child_count: 1,
                        children: vec![ferro_ipc::SizeNode {
                            name: "x.bin".into(),
                            size: 20,
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    ferro_ipc::SizeNode {
                        name: "b.txt".into(),
                        size: 10,
                        ..Default::default()
                    },
                ],
                rest: Some(SizeRest {
                    count: 3,
                    size: 1,
                    ..Default::default()
                }),
                ..Default::default()
            },
            r"C:\".into(),
        );
        let mut rows = Vec::new();
        flatten(&root, 0, Measure::Size, &mut rows);
        assert_eq!(rows.len(), 3, "a, b.txt, rest");
        root.find_mut(r"C:\a").unwrap().expanded = true;
        let mut rows = Vec::new();
        flatten(&root, 0, Measure::Size, &mut rows);
        assert_eq!(rows.len(), 4);
        assert!(matches!(rows[1], Flat::Node { depth: 1, node, .. } if node.name == "x.bin"));
    }
}
