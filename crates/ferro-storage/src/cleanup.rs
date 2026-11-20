//! The cleanup tool (SPEC §5.4): developer artifacts found in the index, package caches
//! and Windows temporary data at well-known paths, and virtual disks for information.
//! Only your temporary files are selected by default; everything is cleaned after a
//! preview, with the safety rules of `cleaner`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, FontId, Key, Rect, RichText, ScrollArea, Sense, TextEdit, pos2, vec2,
};
use ferro_ipc::{
    ArtifactKind, CleanupScanRequest, Request, Response, SearchRequest, SizeTreeRequest, Sort,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
use windows::core::w;

use crate::actions::{ActionLog, Disposal};
use crate::cleaner::{self, Outcome, Target};
use crate::elevation;
use crate::format;
use crate::locations::{self, Category, Location, Risk, Scope};
use crate::paint::{fit_text, row_background};
use crate::shell;
use crate::worker::{Job, JobKind, Outcome as JobOutcome, ServiceState};

const ROW_HEIGHT: f32 = 24.0;
/// Developer artifacts smaller than this are not listed.
const MIN_ARTIFACT: u64 = 1 << 20;
/// The journal needs a moment to report deletions before sizes are measured again.
const RESCAN_DELAY: Duration = Duration::from_millis(1500);

/// One row of the list.
#[derive(Debug, Clone)]
struct Item {
    category: Category,
    label: String,
    path: String,
    scope: Scope,
    size: u64,
    files: u64,
    mtime: u64,
    risk: Risk,
    needs_admin: bool,
    report_only: bool,
    selected: bool,
    note: String,
    marker: String,
}

impl Item {
    /// Can be cleaned by this process.
    fn actionable(&self, elevated: bool) -> bool {
        !self.report_only && (!self.needs_admin || elevated)
    }
}

#[derive(Default)]
struct ActProgress {
    done: AtomicU64,
}

enum Stage {
    Idle,
    Scanning,
    Acting {
        progress: Arc<ActProgress>,
        total: usize,
        result: Receiver<Vec<Outcome>>,
    },
    /// Waiting before measuring again after a cleanup.
    Settling(Instant),
}

struct Preview {
    /// Indexes into `items`.
    items: Vec<usize>,
    permanent: bool,
}

/// Sizes before a cleanup, to report what it freed once the rescan arrives.
struct Before {
    sizes: HashMap<String, u64>,
    summary: String,
}

enum UiAction {
    Toggle(usize),
    ToggleCategory(Category),
    Collapse(Category),
    Reveal(String),
    Open(String),
    CopyPath(String),
}

pub struct Cleanup {
    jobs: Sender<Job>,
    outcomes: Receiver<JobOutcome>,
    ctx: egui::Context,
    service: ServiceState,
    generation: u64,
    folder: String,
    include_venv: bool,
    elevated: bool,
    locations: Vec<Location>,
    items: Vec<Item>,
    /// Responses still expected for the current scan.
    pending: usize,
    scanned_at: Option<Instant>,
    scan_ms: u128,
    complete: bool,
    stage: Stage,
    preview: Option<Preview>,
    before: Option<Before>,
    collapsed: HashSet<Category>,
    dry_run_only: bool,
    log: Option<ActionLog>,
    message: Option<String>,
    error: Option<String>,
    actions: Vec<UiAction>,
}

impl Cleanup {
    pub fn new(
        ctx: egui::Context,
        jobs: Sender<Job>,
        outcomes: Receiver<JobOutcome>,
        folder: String,
        dry_run_only: bool,
    ) -> Self {
        Self {
            jobs,
            outcomes,
            ctx,
            service: ServiceState::Connected,
            generation: 0,
            folder,
            include_venv: false,
            elevated: elevation::is_elevated(),
            locations: locations::known(|name| std::env::var(name).ok()),
            items: Vec::new(),
            pending: 0,
            scanned_at: None,
            scan_ms: 0,
            complete: true,
            stage: Stage::Idle,
            preview: None,
            before: None,
            collapsed: HashSet::new(),
            dry_run_only,
            log: ActionLog::default_file().map(ActionLog::new),
            message: None,
            error: None,
            actions: Vec::new(),
        }
    }

    fn busy(&self) -> bool {
        !matches!(self.stage, Stage::Idle)
    }

    fn send(&mut self, kind: JobKind, request: Request) {
        let job = Job {
            generation: self.generation,
            kind,
            request,
        };
        if self.jobs.send(job).is_ok() {
            self.pending += 1;
        } else {
            self.error = Some("the background worker stopped".into());
        }
    }

    /// Measures every location and searches for artifacts and virtual disks.
    pub fn scan(&mut self) {
        self.generation += 1;
        self.items.clear();
        self.pending = 0;
        self.complete = true;
        self.error = None;
        if self.before.is_none() {
            self.message = None;
        }
        self.scanned_at = Some(Instant::now());
        let folder = self.folder.trim().trim_matches('"').to_owned();
        self.send(
            JobKind::Artifacts,
            Request::CleanupScan(CleanupScanRequest {
                roots: if folder.is_empty() {
                    Vec::new()
                } else {
                    vec![folder]
                },
                include_venv: self.include_venv,
                min_size: MIN_ARTIFACT,
            }),
        );
        for i in 0..self.locations.len() {
            let path = self.locations[i].path.clone();
            self.send(
                JobKind::Location(i),
                Request::SizeTree(SizeTreeRequest::new(path, 0)),
            );
        }
        self.send(
            JobKind::VirtualDisks,
            Request::Search(SearchRequest {
                query: "ext:vhdx".into(),
                sort: Sort::Size,
                descending: true,
                offset: 0,
                limit: 50,
            }),
        );
        self.stage = Stage::Scanning;
    }

    fn receive(&mut self) {
        while let Ok(outcome) = self.outcomes.try_recv() {
            if outcome.generation != self.generation {
                continue;
            }
            self.service = outcome.service.clone();
            self.pending = self.pending.saturating_sub(1);
            match (outcome.kind, outcome.result) {
                (JobKind::Artifacts, Ok(Response::CleanupScan(scan))) => {
                    self.complete &= scan.complete;
                    for a in scan.artifacts {
                        let (label, risk, note) = artifact_info(a.kind);
                        self.items.push(Item {
                            category: Category::Developer,
                            label: label.to_owned(),
                            path: a.path,
                            scope: Scope::Folder,
                            size: a.size,
                            files: a.files,
                            mtime: a.mtime,
                            risk,
                            needs_admin: false,
                            report_only: false,
                            selected: false,
                            note: note.to_owned(),
                            marker: a.marker,
                        });
                    }
                }
                (JobKind::Location(i), Ok(Response::SizeTree(tree))) => {
                    let Some(location) = self.locations.get(i) else {
                        continue;
                    };
                    self.complete &= tree.complete;
                    if tree.root.size == 0 && tree.root.files == 0 {
                        continue; // empty: nothing to clean
                    }
                    self.items.push(Item {
                        category: location.category,
                        label: location.label.to_owned(),
                        path: tree.path,
                        scope: location.scope,
                        size: tree.root.size,
                        files: tree.root.files,
                        mtime: tree.root.mtime,
                        risk: location.risk,
                        needs_admin: location.needs_admin,
                        report_only: location.report_only,
                        selected: location.selected_by_default
                            && (!location.needs_admin || self.elevated),
                        note: location.note.to_owned(),
                        marker: String::new(),
                    });
                }
                (JobKind::VirtualDisks, Ok(Response::Search(found))) => {
                    for row in found.rows {
                        let lower = row.path.to_lowercase();
                        // Read-only images shipped inside applications never grow.
                        if lower.contains("\\program files") || lower.contains("\\resources\\") {
                            continue;
                        }
                        let label = if lower.contains("docker") {
                            "Docker disk"
                        } else if lower.contains("\\packages\\") && lower.contains("\\localstate\\")
                        {
                            "WSL distribution disk"
                        } else {
                            "Virtual disk"
                        };
                        self.items.push(Item {
                            category: Category::VirtualDisks,
                            label: label.to_owned(),
                            path: row.path,
                            scope: Scope::File,
                            size: row.size,
                            files: 1,
                            mtime: row.mtime,
                            risk: Risk::High,
                            needs_admin: true,
                            report_only: true,
                            selected: false,
                            note:
                                "Listed for its size. Dynamic disks do not shrink by themselves; \
                                   compacting needs WSL / Docker stopped and administrator rights \
                                   (not offered yet)."
                                    .to_owned(),
                            marker: String::new(),
                        });
                    }
                }
                // A missing location is simply not listed.
                (JobKind::Location(_), Err(_)) => {}
                (_, Err(message)) => self.error = Some(message),
                (_, Ok(other)) => self.error = Some(format!("unexpected response {other:?}")),
            }
        }
        if matches!(self.stage, Stage::Scanning) && self.pending == 0 {
            self.stage = Stage::Idle;
            self.scan_ms = self.scanned_at.map_or(0, |t| t.elapsed().as_millis());
            self.items
                .sort_by(|a, b| a.category.cmp(&b.category).then(b.size.cmp(&a.size)));
            self.report_freed();
        }
    }

    /// After a cleanup's rescan: compares sizes with those before it.
    fn report_freed(&mut self) {
        let Some(before) = self.before.take() else {
            return;
        };
        let now: HashMap<&str, u64> = self
            .items
            .iter()
            .map(|i| (i.path.as_str(), i.size))
            .collect();
        let freed: u64 = before
            .sizes
            .iter()
            .map(|(path, size)| size.saturating_sub(now.get(path.as_str()).copied().unwrap_or(0)))
            .sum();
        self.message = Some(format!("{} Freed {}.", before.summary, format::size(freed)));
    }

    fn poll(&mut self) {
        match &self.stage {
            Stage::Acting { result, .. } => match result.try_recv() {
                Ok(outcomes) => self.finish(outcomes),
                Err(_) => self.ctx.request_repaint_after(Duration::from_millis(100)),
            },
            Stage::Settling(since) => {
                if since.elapsed() >= RESCAN_DELAY {
                    self.stage = Stage::Idle;
                    self.scan();
                } else {
                    self.ctx.request_repaint_after(Duration::from_millis(100));
                }
            }
            Stage::Idle | Stage::Scanning => {}
        }
    }

    fn selection(&self) -> (usize, u64) {
        self.items
            .iter()
            .filter(|i| i.selected && i.actionable(self.elevated))
            .fold((0, 0), |(n, bytes), i| (n + 1, bytes + i.size))
    }

    fn open_preview(&mut self) {
        let items: Vec<usize> = (0..self.items.len())
            .filter(|&i| self.items[i].selected && self.items[i].actionable(self.elevated))
            .collect();
        if items.is_empty() {
            self.message = Some("Select something to clean first.".into());
            return;
        }
        self.preview = Some(Preview {
            items,
            permanent: false,
        });
    }

    fn run(&mut self, dry_run: bool) {
        let Some(preview) = self.preview.take() else {
            return;
        };
        let Some(log) = self.log.clone() else {
            self.error = Some("LOCALAPPDATA is not set: cannot write the action log".into());
            return;
        };
        let dry_run = dry_run || self.dry_run_only;
        let disposal = if preview.permanent {
            Disposal::Permanent
        } else {
            Disposal::RecycleBin
        };
        let targets: Vec<Target> = preview
            .items
            .iter()
            .map(|&i| {
                let item = &self.items[i];
                Target {
                    label: item.label.clone(),
                    path: item.path.clone(),
                    scope: item.scope,
                    marker: item.marker.clone(),
                    size: item.size,
                }
            })
            .collect();
        self.before = (!dry_run).then(|| Before {
            sizes: targets.iter().map(|t| (t.path.clone(), t.size)).collect(),
            summary: String::new(),
        });
        let progress = Arc::new(ActProgress::default());
        let (tx, rx) = channel();
        let total = targets.len();
        let (thread_progress, ctx) = (Arc::clone(&progress), self.ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("cleanup-actions".into())
            .spawn(move || {
                // SAFETY: first COM call on this new thread; balanced by CoUninitialize below.
                let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                let owner = own_window();
                let outcomes =
                    cleaner::execute(&targets, disposal, dry_run, &log, owner, |i, _| {
                        thread_progress.done.store(i as u64 + 1, Ordering::Relaxed);
                    });
                if com.is_ok() {
                    // SAFETY: matches the successful CoInitializeEx on this thread.
                    unsafe { CoUninitialize() };
                }
                let _ = tx.send(outcomes);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.stage = Stage::Acting {
                    progress,
                    total,
                    result: rx,
                };
            }
            Err(err) => {
                self.before = None;
                self.error = Some(format!("cannot start: {err}"));
            }
        }
    }

    fn finish(&mut self, outcomes: Vec<Outcome>) {
        let (mut done, mut logged, mut partial, mut problems) = (0, 0, 0, Vec::new());
        for outcome in &outcomes {
            match outcome {
                Outcome::Done { left: 0 } => done += 1,
                Outcome::Done { .. } => partial += 1,
                Outcome::Logged => logged += 1,
                Outcome::Skipped(reason) | Outcome::Failed(reason) => problems.push(reason.clone()),
            }
        }
        let mut summary = if logged > 0 {
            format!(
                "Dry run: {} logged, nothing changed.",
                format::counted(logged, "location", None)
            )
        } else {
            format!(
                "Cleaned {}.",
                format::counted(done + partial, "location", None)
            )
        };
        if partial > 0 {
            summary.push_str(&format!(" {partial} had items in use that were left."));
        }
        if let Some(log) = &self.log {
            summary.push_str(&format!(" Log: {}.", log.path().display()));
        }
        self.error = (!problems.is_empty()).then(|| {
            format!(
                "{} not cleaned. First: {}",
                format::counted(problems.len() as u64, "location", None),
                problems[0]
            )
        });
        match &mut self.before {
            Some(before) => {
                before.summary = summary;
                // Measure again once the journal has caught up.
                self.stage = Stage::Settling(Instant::now());
            }
            None => {
                self.message = Some(summary);
                self.stage = Stage::Idle;
            }
        }
    }

    fn apply_actions(&mut self, ctx: &egui::Context) {
        for action in std::mem::take(&mut self.actions) {
            match action {
                UiAction::Toggle(i) => {
                    let elevated = self.elevated;
                    if let Some(item) = self.items.get_mut(i)
                        && item.actionable(elevated)
                    {
                        item.selected = !item.selected;
                    }
                }
                UiAction::ToggleCategory(category) => {
                    let elevated = self.elevated;
                    let all = self
                        .items
                        .iter()
                        .filter(|i| i.category == category && i.actionable(elevated))
                        .all(|i| i.selected);
                    for item in self.items.iter_mut() {
                        if item.category == category && item.actionable(elevated) {
                            item.selected = !all;
                        }
                    }
                }
                UiAction::Collapse(category) => {
                    if !self.collapsed.remove(&category) {
                        self.collapsed.insert(category);
                    }
                }
                UiAction::Reveal(path) => {
                    if let Err(err) = shell::reveal(&path) {
                        self.error = Some(format!("{err:#}"));
                    }
                }
                UiAction::Open(path) => {
                    if let Err(err) = shell::open(&path) {
                        self.error = Some(format!("{err:#}"));
                    }
                }
                UiAction::CopyPath(path) => {
                    ctx.copy_text(path.clone());
                    self.message = Some(format!("Copied {path}"));
                }
            }
        }
    }

    pub fn update(&mut self, ctx: &egui::Context) {
        self.receive();
        self.poll();
        egui::TopBottomPanel::top("cleanup-toolbar").show(ctx, |ui| self.toolbar(ui));
        egui::TopBottomPanel::bottom("cleanup-status").show(ctx, |ui| self.status(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.service == ServiceState::Connected {
                self.list(ui);
            } else {
                self.service_missing(ui);
            }
        });
        self.preview_dialog(ctx);
        self.apply_actions(ctx);
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let folder = ui.add(
                TextEdit::singleline(&mut self.folder)
                    .hint_text("Look for build output in (empty = all volumes)")
                    .desired_width(340.0),
            );
            let enter = folder.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.checkbox(
                &mut self.include_venv,
                "Include Python virtual environments",
            );
            if ui
                .add_enabled(!self.busy(), egui::Button::new("Scan"))
                .clicked()
                || (enter && !self.busy())
            {
                self.scan();
            }
            ui.separator();
            let (count, bytes) = self.selection();
            if ui
                .add_enabled(
                    !self.busy() && count > 0,
                    egui::Button::new("Clean selected…"),
                )
                .on_hover_text("Shows what will be removed before anything happens")
                .clicked()
            {
                self.open_preview();
            }
            if count > 0 {
                ui.label(format!(
                    "{} selected ({})",
                    format::counted(count as u64, "location", None),
                    format::size(bytes)
                ));
            }
            let locked =
                !self.elevated && self.items.iter().any(|i| i.needs_admin && !i.report_only);
            if locked
                && ui
                    .button("Restart as administrator")
                    .on_hover_text("Needed for the locations under C:\\Windows")
                    .clicked()
            {
                let mut args = String::from("--cleanup");
                if self.dry_run_only {
                    args.push_str(" --dry-run");
                }
                match elevation::relaunch_elevated(&args) {
                    Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(err) => self.error = Some(format!("{err:#}")),
                }
            }
            if self.dry_run_only {
                ui.label(
                    RichText::new("--dry-run: actions are only logged")
                        .color(ui.visuals().warn_fg_color),
                );
            }
        });
        ui.add_space(2.0);
    }

    fn status(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            match &self.stage {
                Stage::Scanning => {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label("Measuring…");
                }
                Stage::Settling(_) => {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label("Measuring again…");
                }
                Stage::Acting {
                    progress, total, ..
                } => {
                    let done = progress.done.load(Ordering::Relaxed);
                    ui.add(
                        egui::ProgressBar::new(format::share(done, *total as u64))
                            .desired_width(240.0)
                            .text(format!("Cleaning {done} / {total}")),
                    );
                }
                Stage::Idle => {
                    if self.scanned_at.is_some() {
                        let total: u64 = self
                            .items
                            .iter()
                            .filter(|i| !i.report_only)
                            .map(|i| i.size)
                            .sum();
                        ui.label(
                            RichText::new(format!(
                                "{} cleanable in {} ({} ms)",
                                format::size(total),
                                format::counted(
                                    self.items.iter().filter(|i| !i.report_only).count() as u64,
                                    "location",
                                    None
                                ),
                                self.scan_ms
                            ))
                            .weak(),
                        );
                    }
                }
            }
            if !self.complete {
                ui.label(RichText::new("indexing not finished").color(ui.visuals().warn_fg_color));
            }
            if let Some(error) = &self.error {
                ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            }
            if let Some(message) = &self.message {
                ui.label(message);
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
                self.scan();
            }
        });
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        if self.items.is_empty() {
            if !self.busy() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.label(
                        RichText::new(
                            "Press Scan to measure temporary files, caches and build output.",
                        )
                        .weak(),
                    );
                    ui.label(
                        RichText::new("Nothing is removed until you confirm a preview.").weak(),
                    );
                });
            }
            return;
        }
        // Header line per category, then its items unless collapsed.
        let mut lines: Vec<Line> = Vec::new();
        for category in Category::ALL {
            let members: Vec<usize> = (0..self.items.len())
                .filter(|&i| self.items[i].category == category)
                .collect();
            if members.is_empty() {
                continue;
            }
            lines.push(Line::Header(category));
            if !self.collapsed.contains(&category) {
                lines.extend(members.into_iter().map(Line::Item));
            }
        }
        let Self {
            items,
            actions,
            collapsed,
            elevated,
            ..
        } = self;
        let elevated = *elevated;
        ScrollArea::vertical().auto_shrink([false; 2]).show_rows(
            ui,
            ROW_HEIGHT,
            lines.len(),
            |ui, range| {
                for line in &lines[range] {
                    let (rect, response) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ROW_HEIGHT),
                        Sense::click(),
                    );
                    match *line {
                        Line::Header(category) => {
                            let members: Vec<&Item> =
                                items.iter().filter(|i| i.category == category).collect();
                            let actionable: Vec<&&Item> =
                                members.iter().filter(|i| i.actionable(elevated)).collect();
                            let state = if actionable.is_empty() {
                                None
                            } else {
                                Some(actionable.iter().all(|i| i.selected))
                            };
                            paint_header(
                                ui,
                                rect,
                                category,
                                &members,
                                collapsed.contains(&category),
                                state,
                            );
                            let on_box = response.interact_pointer_pos().is_some_and(|p| {
                                p.x < rect.left() + 40.0 && p.x > rect.left() + 22.0
                            });
                            if response.clicked() {
                                actions.push(if on_box && state.is_some() {
                                    UiAction::ToggleCategory(category)
                                } else {
                                    UiAction::Collapse(category)
                                });
                            }
                        }
                        Line::Item(i) => {
                            let item = &items[i];
                            paint_item(ui, rect, item, elevated, response.hovered());
                            let response = response.on_hover_text(format!(
                                "{}\n{}, {}\n{}",
                                item.path,
                                format::size(item.size),
                                format::counted(item.files, "file", None),
                                item.note
                            ));
                            if response.double_clicked() {
                                actions.push(UiAction::Reveal(item.path.clone()));
                            } else if response.clicked() {
                                actions.push(UiAction::Toggle(i));
                            }
                            response.context_menu(|ui| {
                                if ui.button("Show in Explorer").clicked() {
                                    actions.push(UiAction::Reveal(item.path.clone()));
                                }
                                if ui.button("Open").clicked() {
                                    actions.push(UiAction::Open(item.path.clone()));
                                }
                                if ui.button("Copy path").clicked() {
                                    actions.push(UiAction::CopyPath(item.path.clone()));
                                }
                            });
                        }
                    }
                }
            },
        );
    }

    fn preview_dialog(&mut self, ctx: &egui::Context) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        let mut choice = None;
        let dry_run_only = self.dry_run_only;
        let items = &self.items;
        egui::Modal::new(egui::Id::new("cleanup-preview")).show(ctx, |ui| {
            ui.set_width(760.0);
            let bytes: u64 = preview.items.iter().map(|&i| items[i].size).sum();
            let count = format::counted(preview.items.len() as u64, "location", None);
            ui.heading(if preview.permanent {
                format!("Permanently delete {count}")
            } else {
                format!("Move {count} to the Recycle Bin")
            });
            ui.label(format!(
                "About {} in total. Each location is checked again right before it is cleaned; \
                 files in use are left alone.",
                format::size(bytes)
            ));
            // Same height for both texts, so toggling the checkbox below does not move the
            // buttons under the pointer.
            ui.scope(|ui| {
                ui.set_min_height(40.0);
                let (text, color) = if preview.permanent {
                    (
                        "Permanently deleted files skip the Recycle Bin and cannot be restored. \
                         Make sure nothing listed here is still needed.",
                        ui.visuals().error_fg_color,
                    )
                } else {
                    (
                        "The Recycle Bin frees disk space only once it is emptied. Very large folders may not fit; \
                         Windows then asks before deleting them permanently.",
                        ui.visuals().warn_fg_color,
                    )
                };
                ui.label(RichText::new(text).color(color));
            });
            ui.add_space(6.0);
            ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for &i in &preview.items {
                    let item = &items[i];
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&item.label).strong());
                        ui.label(RichText::new(format!("({}, {})", item.scope.describe(), format::size(item.size))).weak());
                    });
                    ui.label(RichText::new(format!("    {}", item.path)).weak());
                }
            });
            ui.add_space(6.0);
            ui.checkbox(
                &mut preview.permanent,
                "Delete permanently instead of using the Recycle Bin (cannot be undone)",
            );
            if dry_run_only {
                ui.label(
                    RichText::new("Started with --dry-run: actions are only logged.")
                        .color(ui.visuals().warn_fg_color),
                );
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!dry_run_only, egui::Button::new("Apply")).clicked() {
                    choice = Some(Some(false));
                }
                if ui.button("Dry run (log only)").clicked() {
                    choice = Some(Some(true));
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(None);
                }
            });
        });
        match choice {
            Some(Some(dry_run)) => self.run(dry_run),
            Some(None) => self.preview = None,
            None => {}
        }
    }
}

enum Line {
    Header(Category),
    Item(usize),
}

/// Label, risk and explanation of a developer artifact.
fn artifact_info(kind: ArtifactKind) -> (&'static str, Risk, &'static str) {
    match kind {
        ArtifactKind::NodeModules => (
            "npm dependencies (node_modules)",
            Risk::Low,
            "Restored with npm / yarn / pnpm install.",
        ),
        ArtifactKind::RustTarget => (
            "Rust build output (target)",
            Risk::Low,
            "Rebuilt by cargo build (the first build takes longer).",
        ),
        ArtifactKind::MavenTarget => (
            "Maven build output (target)",
            Risk::Low,
            "Rebuilt by mvn package.",
        ),
        ArtifactKind::GradleCache => (
            "Gradle project cache (.gradle)",
            Risk::Low,
            "Recreated by the next Gradle build.",
        ),
        ArtifactKind::Build => (
            "Build output (build)",
            Risk::Medium,
            "Usually generated; some projects keep hand-written files in build/, so check first.",
        ),
        ArtifactKind::Dist => (
            "Distribution output (dist)",
            Risk::Medium,
            "Usually generated by the build; check that it is not committed by hand.",
        ),
        ArtifactKind::PyCache => (
            "Python bytecode (__pycache__)",
            Risk::Low,
            "Python recreates it on the next run.",
        ),
        ArtifactKind::PythonVenv => (
            "Python virtual environment",
            Risk::Medium,
            "Packages must be installed again (pip install -r requirements.txt or similar).",
        ),
    }
}

fn risk_color(risk: Risk, visuals: &egui::Visuals) -> Color32 {
    match risk {
        Risk::Low => Color32::from_rgb(80, 170, 100),
        Risk::Medium => visuals.warn_fg_color,
        Risk::High => visuals.error_fg_color,
    }
}

fn paint_header(
    ui: &egui::Ui,
    rect: Rect,
    category: Category,
    members: &[&Item],
    collapsed: bool,
    state: Option<bool>,
) {
    let visuals = ui.visuals();
    ui.painter().rect_filled(
        rect.shrink2(vec2(0.0, 1.0)),
        2.0,
        visuals.widgets.noninteractive.weak_bg_fill,
    );
    // Collapse triangle.
    let c = pos2(rect.left() + 11.0, rect.center().y);
    let points = if collapsed {
        vec![
            pos2(c.x - 2.0, c.y - 4.0),
            pos2(c.x - 2.0, c.y + 4.0),
            pos2(c.x + 3.0, c.y),
        ]
    } else {
        vec![
            pos2(c.x - 4.0, c.y - 2.0),
            pos2(c.x + 4.0, c.y - 2.0),
            pos2(c.x, c.y + 3.0),
        ]
    };
    ui.painter().add(egui::Shape::convex_polygon(
        points,
        visuals.text_color(),
        egui::Stroke::NONE,
    ));
    if let Some(all) = state {
        let check =
            Rect::from_center_size(pos2(rect.left() + 31.0, rect.center().y), vec2(13.0, 13.0));
        paint_checkbox(ui, check, all, true);
    }
    let size: u64 = members.iter().map(|i| i.size).sum();
    ui.painter().text(
        pos2(rect.left() + 46.0, rect.center().y),
        Align2::LEFT_CENTER,
        format!(
            "{}  ·  {}  ·  {}",
            category.title(),
            format::size(size),
            format::counted(members.len() as u64, "item", None)
        ),
        FontId::proportional(13.5),
        visuals.strong_text_color(),
    );
}

fn paint_item(ui: &egui::Ui, rect: Rect, item: &Item, elevated: bool, hovered: bool) {
    row_background(ui, rect, false, hovered);
    let visuals = ui.visuals();
    let actionable = item.actionable(elevated);
    let check = Rect::from_center_size(pos2(rect.left() + 31.0, rect.center().y), vec2(13.0, 13.0));
    if !item.report_only {
        paint_checkbox(ui, check, item.selected && actionable, actionable);
    }
    let font = FontId::proportional(13.5);
    let right_block = 330.0;
    let label_left = check.right() + 10.0;
    let label_width = 260.0_f32.min((rect.right() - right_block - label_left) * 0.4);
    let label_rect = Rect::from_x_y_ranges(label_left..=label_left + label_width, rect.y_range());
    let strong = if actionable || item.report_only {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    ui.painter().with_clip_rect(label_rect).text(
        label_rect.left_center(),
        Align2::LEFT_CENTER,
        fit_text(ui, &item.label, &font, label_rect.width(), false),
        font.clone(),
        strong,
    );
    let path_rect = Rect::from_x_y_ranges(
        label_rect.right() + 10.0..=rect.right() - right_block,
        rect.y_range(),
    );
    if path_rect.width() > 10.0 {
        ui.painter().with_clip_rect(path_rect).text(
            path_rect.left_center(),
            Align2::LEFT_CENTER,
            fit_text(
                ui,
                &item.path,
                &FontId::proportional(12.5),
                path_rect.width(),
                true,
            ),
            FontId::proportional(12.5),
            visuals.weak_text_color(),
        );
    }
    let x = rect.right() - right_block;
    ui.painter().text(
        pos2(x + 80.0, rect.center().y),
        Align2::RIGHT_CENTER,
        format::size(item.size),
        font,
        strong,
    );
    ui.painter().text(
        pos2(x + 92.0, rect.center().y),
        Align2::LEFT_CENTER,
        format::date(item.mtime),
        FontId::proportional(12.5),
        visuals.weak_text_color(),
    );
    let (badge, color) = if item.report_only {
        ("info only", visuals.weak_text_color())
    } else if item.needs_admin && !elevated {
        ("needs admin", visuals.warn_fg_color)
    } else {
        (item.risk.label(), risk_color(item.risk, visuals))
    };
    ui.painter().text(
        pos2(rect.right() - 8.0, rect.center().y),
        Align2::RIGHT_CENTER,
        badge,
        FontId::proportional(12.5),
        color,
    );
}

fn paint_checkbox(ui: &egui::Ui, rect: Rect, checked: bool, enabled: bool) {
    let visuals = ui.visuals();
    let fill = if checked {
        visuals.selection.bg_fill
    } else {
        visuals.extreme_bg_color
    };
    let stroke = if enabled {
        visuals.widgets.inactive.fg_stroke
    } else {
        egui::Stroke::new(1.0_f32, visuals.weak_text_color().gamma_multiply(0.5))
    };
    ui.painter().rect_filled(rect, 2.0, fill);
    ui.painter()
        .rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
    if checked {
        let stroke = egui::Stroke::new(2.0_f32, visuals.selection.stroke.color);
        let (a, b, c) = (
            pos2(rect.left() + 3.0, rect.center().y),
            pos2(rect.center().x - 1.0, rect.bottom() - 3.0),
            pos2(rect.right() - 3.0, rect.top() + 3.0),
        );
        ui.painter().line_segment([a, b], stroke);
        ui.painter().line_segment([b, c], stroke);
    }
}

/// The storage window, as owner for shell dialogs.
fn own_window() -> Option<HWND> {
    // SAFETY: the title is a static NUL-terminated literal.
    unsafe { FindWindowW(None, w!("ferrofs storage")) }.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(category: Category, needs_admin: bool, report_only: bool) -> Item {
        Item {
            category,
            label: "x".into(),
            path: r"C:\a\b".into(),
            scope: Scope::Contents,
            size: 1,
            files: 1,
            mtime: 0,
            risk: Risk::Low,
            needs_admin,
            report_only,
            selected: false,
            note: String::new(),
            marker: String::new(),
        }
    }

    #[test]
    fn admin_and_report_only_items() {
        assert!(item(Category::Windows, false, false).actionable(false));
        assert!(!item(Category::Windows, true, false).actionable(false));
        assert!(item(Category::Windows, true, false).actionable(true));
        assert!(!item(Category::VirtualDisks, true, true).actionable(true));
        let (_, risk, _) = artifact_info(ArtifactKind::Build);
        assert_eq!(risk, Risk::Medium);
    }
}
